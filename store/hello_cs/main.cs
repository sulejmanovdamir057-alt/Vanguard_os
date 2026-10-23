// Пример приложения формата apps/<id> на C#: ядро компилирует этот файл через
// csc.exe (.NET Framework, входит в каждую Windows) и запускает отдельным
// процессом. Флаг winexe ядро подбирает по строке System.Windows.Forms —
// иначе рядом с окном открылась бы консоль. Код — строго C# 5: это компилятор
// у Framework, без SDK.
using System;
using System.Drawing;
using System.Windows.Forms;

namespace Vanguard.HelloCs
{
    internal static class Program
    {
        [STAThread]
        private static void Main()
        {
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);

            Form form = new Form
            {
                Text = "Vanguard OS — C#",
                Width = 440,
                Height = 230,
                StartPosition = FormStartPosition.CenterScreen,
                BackColor = Color.FromArgb(24, 24, 28),
                FormBorderStyle = FormBorderStyle.FixedSingle,
                MaximizeBox = false,
            };

            Label label = new Label
            {
                Text = "Привет из C#!\n\nЭто настоящее окно Windows,\nскомпилированное через csc.",
                Dock = DockStyle.Fill,
                ForeColor = Color.White,
                Font = new Font("Segoe UI", 12f),
                TextAlign = ContentAlignment.MiddleCenter,
            };

            form.Controls.Add(label);
            Application.Run(form);
        }
    }
}
