using System;
using System.Diagnostics;
using System.IO;
using System.Threading;

// Local-only supervisor fixture. It never connects to a relay.
public class FakePortal
{
    public static int Main(string[] args)
    {
        // Match the real Rust CLI, including under a hidden OEM-codepage host.
        Console.OutputEncoding = new System.Text.UTF8Encoding(false);
        if (args.Length > 0 && args[0] == "--install-user-runtime")
        {
            // Lifecycle fixtures already own an isolated installation.
            string binary = Process.GetCurrentProcess().MainModule.FileName;
            string installRoot = Path.GetDirectoryName(binary);
            if (Path.GetFileName(installRoot) == "release" && Path.GetFileName(Path.GetDirectoryName(installRoot)) == "target")
                installRoot = Path.GetDirectoryName(Path.GetDirectoryName(installRoot));
            Console.WriteLine("{\"root\":\"" + installRoot.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"}");
            return 0;
        }
        if (args.Length > 0 && args[0] == "--version")
        {
            Console.WriteLine("heart-portal 0.8.0");
            return 0;
        }
        if (args.Length > 1 && args[0] == "config")
        {
            // Explicit fixture path only: never touch the developer's real config.
            string config = Environment.GetEnvironmentVariable("HEART_PORTAL_FIXTURE_CONFIG");
            if (String.IsNullOrEmpty(config)) return 88;
            if (args[1] == "init" && !File.Exists(config)) {
                Directory.CreateDirectory(Path.GetDirectoryName(config));
                File.WriteAllText(config, "# isolated supervisor fixture\n");
            }
            Console.WriteLine("{\"config\":{\"path\":\"" + config.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"}}");
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
        while (true) {
            Console.WriteLine("fixture heartbeat");
            Console.Error.WriteLine("fixture stderr heartbeat");
            Thread.Sleep(100);
        }
    }
}
