// vlaunch.cs - Vanguard OS launch bridge, compiled on demand by
// src/core/launcher.rs with the .NET Framework csc (ships with Windows,
// no SDK and no NuGet involved). Invoked as:
//
//   vlaunch runas <file> [args]  - ShellExecute verb "runas" (UAC prompt);
//   vlaunch aumid <AUMID>        - UWP/system app via explorer shell:AppsFolder\...;
//   vlaunch clsid <GUID>         - shell::{GUID} object (braces optional);
//   vlaunch shell <target>       - plain ShellExecute (URL, file, folder).
//
// Prints "pid=<n>" on stdout for the Rust caller; errors go to stderr with
// a non-zero exit code. KEEP THIS FILE ASCII-ONLY: same discipline as
// tools/*.ps1 (PS 5.1 reads BOM-less scripts in the ANSI codepage) - here it
// keeps the source readable in any editor, and /codepage:65001 does the rest.

using System;
using System.Diagnostics;

internal static class VLaunch
{
    private static int Main(string[] args)
    {
        if (args.Length < 2)
        {
            Console.Error.WriteLine("usage: vlaunch runas|aumid|clsid|shell <target> [args]");
            return 2;
        }

        try
        {
            string mode = args[0].ToLowerInvariant();
            string target = args[1];
            string rest = args.Length > 2
                ? string.Join(" ", args, 2, args.Length - 2)
                : string.Empty;

            ProcessStartInfo psi;
            if (mode == "runas")
            {
                psi = new ProcessStartInfo(target, rest) { UseShellExecute = true, Verb = "runas" };
            }
            else if (mode == "aumid")
            {
                psi = new ProcessStartInfo("explorer.exe", "shell:AppsFolder\\" + target)
                {
                    UseShellExecute = true
                };
            }
            else if (mode == "clsid")
            {
                string guid = target.Trim('{', '}');
                psi = new ProcessStartInfo("explorer.exe", "shell:::{" + guid + "}")
                {
                    UseShellExecute = true
                };
            }
            else if (mode == "shell")
            {
                psi = new ProcessStartInfo(target, rest) { UseShellExecute = true };
            }
            else
            {
                Console.Error.WriteLine("unknown mode: " + mode);
                return 2;
            }

            // runas already carries its arguments in the constructor above.
            if (mode != "runas" && rest.Length > 0)
            {
                psi.Arguments = rest;
            }

            Process started = Process.Start(psi);
            Console.WriteLine(started == null ? "started" : "pid=" + started.Id);
            return 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine(e.Message);
            return 1;
        }
    }
}
