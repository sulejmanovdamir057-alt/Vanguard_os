# Пример приложения формата apps/<id>/: окно Tkinter отдельным процессом.
try:
    import tkinter as tk

    root = tk.Tk()
    root.title("Hello from Vanguard OS")
    root.geometry("360x160")
    tk.Label(root, text="Привет из магазина Vanguard OS!", font=("Segoe UI", 13)).pack(pady=20)
    tk.Button(root, text="Закрыть", command=root.destroy).pack()
    root.mainloop()
except Exception as e:
    print("hello:", e)
