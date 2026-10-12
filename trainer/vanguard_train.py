#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
Vanguard OS — SELF-FINETUNE PROTOCOL (QLoRA).

Дообучает языковую модель на собственных диалогах приложения.
Журнал пишет src/shell.rs: каждая реплика терминала попадает в
vanguard_dialogues.jsonl (role/content/ts), а этот скрипт превращает
их в обучающие пары (вопрос -> ответ) и гоняет QLoRA-дообучение.

Честное самообучение, а не model collapse — три защитных решения:
  * адаптер Loра НИКОГДА не вливается в базу: сохраняется отдельно
    в trainer/runs/lora_<время>/, применять его или нет — решение оператора;
  * последние 10% пар (минимум 1) уходят в holdout, финальный holdout-loss
    печатается в лог: если он вырос против baseline, дообучение вредное;
  * минимум MIN_PAIRS пар — иначе скрипт отказывается с объяснением.

Перед стартом модель выгружается из Ollama (keep_alive=0): тренировке
нужна вся VRAM (12 ГБ карта -> 4-битные веса 9B-модели влезают ~8 ГБ).

Запуск из приложения (окно Neural Trainer) или руками:
    python trainer/vanguard_train.py --hf-model Qwen/Qwen3.5-9B --steps 30
Быстрый смоук-тест на крошечной модели:
    python trainer/vanguard_train.py --hf-model Qwen/Qwen2.5-0.5B-Instruct --steps 3
"""
import argparse
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

# Журнал диалогов приложения (CWD процесса приложения — корень репозитория).
DATA_FILE = "vanguard_dialogues.jsonl"
OUT_DIR = Path("trainer/runs")
# Меньше этого числа пар дообучаться бессмысленно: loss шумит, адаптер переучится.
MIN_PAIRS = 10


def log(msg: str) -> None:
    """Каждая строка уходит в GUI-чат Vanguard OS (Rust читает stdout построчно)."""
    print(f"[TRAINER] {msg}", flush=True)


def load_pairs(path: str) -> list:
    """Читает JSONL-журнал и собирает пары user->assistant.

    Строго последовательно: ответ модели привязан к предшествующему вопросу,
    интерливинг (вопрос, вопрос, ответ) не склеивается — это были бы готовые
    галлюцинации в данных.
    """
    pairs = []
    pending_user = None
    try:
        f = open(path, encoding="utf-8")
    except FileNotFoundError:
        return pairs
    with f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            role, content = rec.get("role"), rec.get("content")
            if not isinstance(content, str) or not content.strip():
                continue
            if role == "user":
                pending_user = content
            elif role == "assistant" and pending_user is not None:
                pairs.append({"user": pending_user, "assistant": content})
                pending_user = None
    return pairs


def unload_ollama() -> None:
    """Просит Ollama выгрузить текущую модель из VRAM (keep_alive=0).

    Ollama 0.40 принимает keep_alive и числом, и строкой (эта ловушка была
    в ai_core.rs для 30m/-1), здесь шлём число — документированный формат.
    Если демон не отвечает — не фатально: trainer сам честно упадёт на
    нехватке памяти с внятным сообщением.
    """
    model = os.environ.get("VANGUARD_AI_MODEL", "qwen3.5:9b")
    url = os.environ.get("VANGUARD_AI_URL", "http://localhost:11434/v1/chat/completions")
    api = url.split("/v1/")[0] + "/api/generate"
    body = json.dumps({"model": model, "keep_alive": 0}).encode()
    req = urllib.request.Request(api, data=body, headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=15).read()
        log(f"модель {model} выгружена из Ollama — VRAM свободна")
    except Exception as e:  # noqa: BLE001 — демон может быть не запущен, это ок
        log(f"предупреждение: Ollama не ответила ({e}) — продолжаем, но VRAM может не хватить")


def main() -> None:
    ap = argparse.ArgumentParser(description="Vanguard OS QLoRA self-finetune")
    ap.add_argument("--hf-model", default=os.environ.get("VANGUARD_TRAIN_HF_MODEL", "Qwen/Qwen3.5-9B"),
                    help="идентификатор HuggingFace (не Ollama-тег!)")
    ap.add_argument("--steps", type=int, default=30, help="шагов обучения (30 шагов ~ несколько минут)")
    ap.add_argument("--lr", type=float, default=2e-4, help="learning rate (для LoRA обычно 1e-4..2e-4)")
    ap.add_argument("--data", default=DATA_FILE, help="журнал диалогов JSONL")
    args = ap.parse_args()

    import torch
    if not torch.cuda.is_available():
        sys.exit(
            "CUDA недоступна. Нужна CUDA-сборка torch:\n"
            "  pip install --index-url https://download.pytorch.org/whl/cu126 --force-reinstall torch\n"
            "(PyPI-версия по умолчанию — CPU-only, на ней обучение невозможно.)"
        )

    pairs = load_pairs(args.data)
    if len(pairs) < MIN_PAIRS:
        sys.exit(
            f"мало данных: {len(pairs)} пар из {args.data} (нужно >= {MIN_PAIRS}).\n"
            "Просто пообщайтесь с Ви в терминале и запустите дообучение снова."
        )
    # Holdout — последние 10% пар: проверка, что адаптер не выучил шум.
    n_hold = max(1, len(pairs) // 10)
    train_pairs, hold_pairs = pairs[:-n_hold], pairs[-n_hold:]
    log(f"данные: {len(train_pairs)} пар обучение / {len(hold_pairs)} holdout")

    unload_ollama()

    from transformers import (
        AutoModelForCausalLM,
        AutoTokenizer,
        BitsAndBytesConfig,
        Trainer,
        TrainingArguments,
    )
    from peft import LoraConfig, get_peft_model, prepare_model_for_kbit_training

    log(f"загрузка {args.hf_model} в 4-bit (NF4)…")
    tok = AutoTokenizer.from_pretrained(args.hf_model, trust_remote_code=True)
    if tok.pad_token is None:
        tok.pad_token = tok.eos_token
    bnb = BitsAndBytesConfig(
        load_in_4bit=True,
        bnb_4bit_quant_type="nf4",
        bnb_4bit_use_double_quant=True,
        bnb_4bit_compute_dtype=torch.bfloat16,  # 3080 Ti (Ampere) bf16 умеет
    )
    model = AutoModelForCausalLM.from_pretrained(
        args.hf_model, quantization_config=bnb, device_map="cuda:0", dtype=torch.bfloat16
    )
    model.config.use_cache = False
    model = prepare_model_for_kbit_training(model, use_gradient_checkpointing=True)
    lora = LoraConfig(
        r=16,
        lora_alpha=32,
        lora_dropout=0.05,
        bias="none",
        task_type="CAUSAL_LM",
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj"],
    )
    model = get_peft_model(model, lora)
    model.print_trainable_parameters()

    system_prompt = (
        "Ты — Ви, автономный ИИ-агент суверенной операционной системы Vanguard OS. "
        "Отвечай кратко, по-русски, в фирменном стиле."
    )

    def encode(pair):
        """Одна обучающая пара: loss считается ТОЛЬКО на ответе модели,
        промпт маскируется -100 — иначе адаптер учится дублировать вопросы."""
        prompt = tok.apply_chat_template(
            [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": pair["user"]},
            ],
            tokenize=False,
            add_generation_prompt=True,
        )
        text = prompt + pair["assistant"] + (tok.eos_token or "")
        enc = tok(text, truncation=True, max_length=1024, return_tensors="pt")
        prompt_len = len(tok(prompt, truncation=True, max_length=1024)["input_ids"])
        labels = enc["input_ids"].clone()
        labels[0, :prompt_len] = -100
        enc["labels"] = labels
        return enc

    class PairDataset(torch.utils.data.Dataset):
        def __init__(self, items):
            self.items = items

        def __len__(self):
            return len(self.items)

        def __getitem__(self, i):
            return encode(self.items[i])

    run_dir = OUT_DIR / time.strftime("lora_%Y%m%d_%H%M%S")
    targs = TrainingArguments(
        output_dir=str(run_dir),
        max_steps=args.steps,
        per_device_train_batch_size=1,
        gradient_accumulation_steps=4,
        learning_rate=args.lr,
        lr_scheduler_type="cosine",
        # transformers 5.x: warmup_ratio удалён, только warmup_steps (2 из 30 шагов ~ 7%).
        warmup_steps=2,
        logging_steps=1,
        save_strategy="no",
        report_to=[],
        bf16=True,
        optim="paged_adamw_8bit",
    )
    trainer = Trainer(model=model, args=targs, train_dataset=PairDataset(train_pairs))

    log(f"обучение: {args.steps} шагов, lr={args.lr}, gacc=4 (эффективный батч 4)…")
    try:
        result = trainer.train()
    except Exception as e:  # noqa: BLE001 — важен внятный текст в GUI-чате, а не тип
        if "out of memory" in str(e).lower():
            sys.exit(
                "не хватило VRAM: закройте тяжёлые приложения, уменьшите --steps "
                "или возьмите модель поменьше (--hf-model Qwen/Qwen2.5-1.5B-Instruct)."
            )
        raise

    log("оценка holdout…")
    model.eval()
    hold_loss = 0.0
    with torch.no_grad():
        for p in hold_pairs:
            batch = {k: v.to("cuda") for k, v in encode(p).items()}
            hold_loss += model(**batch).loss.item()
    hold_loss /= len(hold_pairs)

    run_dir.mkdir(parents=True, exist_ok=True)
    trainer.save_model(str(run_dir))  # сохраняется только LoRA-адаптер, не база
    summary = {
        "base": args.hf_model,
        "steps": args.steps,
        "train_pairs": len(train_pairs),
        "holdout_pairs": len(hold_pairs),
        "train_loss": round(result.training_loss, 4),
        "holdout_loss": round(hold_loss, 4),
    }
    (run_dir / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    log(f"ГОТОВО: train_loss={result.training_loss:.4f} holdout_loss={hold_loss:.4f}")
    log(f"адаптер сохранён: {run_dir} (база не изменена)")
    log("залить в Ollama можно позже: слить LoRA -> GGUF -> Modelfile; если holdout-loss вырос — адаптер не применяйте")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit("прервано пользователем")
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001 — финальный барьер: любая ошибка -> текст в чат, не стек в пустоту
        sys.exit(f"ТРЕНИРОВКА УПАЛА: {type(e).__name__}: {e}")
