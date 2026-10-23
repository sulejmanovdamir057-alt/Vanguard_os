# Часы — второй пример приложения: живое окно, тикает раз в секунду.
import tkinter as tk
from datetime import datetime

root = tk.Tk()
root.title("Vanguard Clock")
root.geometry("240x110")
lbl = tk.Label(root, font=("Consolas", 26))
lbl.pack(expand=True)


def tick():
    lbl.config(text=datetime.now().strftime("%H:%M:%S"))
    root.after(1000, tick)


tick()
root.mainloop()
