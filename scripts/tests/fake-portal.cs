using System;
using System.Diagnostics;
using System.IO;
using System.Threading;

// Local-only supervisor fixture. It never connects to a relay.
public class FakePortal
{
    public static int Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "--version")
        {
            Console.WriteLine("heart-portal 0.8.0");
            return 0;
        }
        if (args.Length > 1 && args[0] == "--export-windows-runtime")
        {
            Directory.CreateDirectory(args[1]);
            string payload = Path.Combine(Path.GetDirectoryName(Process.GetCurrentProcess().MainModule.FileName), "fixture-support");
            foreach (string file in Directory.GetFiles(payload))
                File.Copy(file, Path.Combine(args[1], Path.GetFileName(file)), true);
            return 0;
        }
        if (args.Length > 0 && args[0] == "--hold-pipes")
        {
            Console.WriteLine("child holding inherited stdout" + new string('x', 8192));
            Console.Error.WriteLine("child holding inherited stderr");
            Thread.Sleep(60000);
            return 0;
        }
        string root = Environment.CurrentDirectory;
        string ready = Environment.GetEnvironmentVariable("HEART_PORTAL_READY_FILE");
        string nonce = Environment.GetEnvironmentVariable("HEART_PORTAL_READY_NONCE");
        if (!String.IsNullOrEmpty(ready))
        {
            File.WriteAllText(ready, "{\"pid\":" + Process.GetCurrentProcess().Id +
                ",\"nonce\":\"" + nonce + "\",\"version\":\"0.8.0\"}");
        }
        File.AppendAllText(Path.Combine(root, "launches.txt"),
            Process.GetCurrentProcess().Id + "|" + String.Join("|", args) + "|" +
            Environment.GetEnvironmentVariable("HEART_PORTAL_SUPERVISED") + Environment.NewLine);
        if (File.Exists(Path.Combine(root, "hold-pipes")))
        {
            var start = new ProcessStartInfo(Process.GetCurrentProcess().MainModule.FileName, "--hold-pipes");
            start.UseShellExecute = false;
            start.CreateNoWindow = true;
            // Force STARTF_USESTDHANDLES so stdout/stderr are inherited even
            // though neither this fixture nor its child has a console window.
            start.RedirectStandardInput = true;
            using (var child = Process.Start(start)) { }
            Thread.Sleep(100);
            return 17;
        }
        while (true) { Thread.Sleep(100); }
    }
}
