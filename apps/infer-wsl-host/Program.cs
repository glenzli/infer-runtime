// Windows owns WSL activation and composes the independently bounded status bridge.
using System;
using System.Diagnostics;
using System.IO;
using System.Security.Principal;
using System.Threading;
using System.Threading.Tasks;

namespace InferWslHost
{
    internal static class Program
    {
        private static readonly object LogGate = new object();
        [STAThread]
        private static int Main(string[] args)
        {
            try
            {
                if (args.Length != 1) throw new ArgumentException("Expected an installed host configuration.");
                var settings = HostSettings.Load(args[0]);
                bool created;
                using (var singleton = new Mutex(true, @"Local\InferRuntime.WslHost." + WindowsIdentity.GetCurrent().User.Value, out created))
                {
                    if (!created) return 0;
                    Directory.CreateDirectory(settings.LogDirectory);
                    Run(settings).GetAwaiter().GetResult();
                }
                return 0;
            }
            catch (Exception error)
            {
                // No raw HTTP errors, local credentials, or application payloads in logs.
                Console.Error.WriteLine("Infer WSL host stopped: " + error.GetType().Name);
                return 1;
            }
        }

        private static async Task Run(HostSettings settings)
        {
            using (var bridge = new StatusBridge(settings))
            {
                // Bind before publishing any offer. A fresh host owns a fresh generation.
                var serving = bridge.Serve();
                Process keepalive = null;
                string lastError = null;
                try
                {
                    while (true)
                    {
                        if (serving.IsCompleted) await serving;
                        try
                        {
                            if (keepalive == null || keepalive.HasExited)
                            {
                                if (keepalive != null)
                                {
                                    Log(settings, "WSL activation exited (" + keepalive.ExitCode + "); reconnecting.");
                                    keepalive.Dispose();
                                }
                                keepalive = Process.Start(new ProcessStartInfo
                                {
                                    FileName = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.System), "wsl.exe"),
                                    Arguments = "--distribution " + WslArgument(settings.Distribution) + " --exec bash " + WslArgument(settings.LinuxInstall + "/current/keepalive.sh"),
                                    UseShellExecute = false,
                                    CreateNoWindow = true,
                                    // A WinExe has no console handles. WSL needs valid
                                    // stdio handles even for a noninteractive command.
                                    RedirectStandardInput = true,
                                    RedirectStandardOutput = true,
                                    RedirectStandardError = true,
                                    WorkingDirectory = settings.LogDirectory
                                });
                                keepalive.OutputDataReceived += (sender, data) => { };
                                keepalive.ErrorDataReceived += (sender, data) => { };
                                keepalive.BeginOutputReadLine();
                                keepalive.BeginErrorReadLine();
                                Log(settings, "WSL activation started (pid " + keepalive.Id + ").");
                            }
                            await bridge.Reconcile();
                            if (lastError != null) Log(settings, "Runtime reachable; Windows offers reconciled.");
                            lastError = null;
                        }
                        catch (Exception error)
                        {
                            var code = error.GetType().Name;
                            if (lastError != code) Log(settings, "Runtime temporarily unavailable (" + code + "); retrying.");
                            lastError = code;
                        }
                        await Task.Delay(3000);
                    }
                }
                finally
                {
                    // Stopping this user host does not kill independently supervised WSL services.
                    if (keepalive != null)
                    {
                        try
                        {
                            keepalive.StandardInput.Close();
                            if (!keepalive.WaitForExit(2500)) keepalive.Kill();
                        }
                        catch (InvalidOperationException) { }
                        keepalive.Dispose();
                    }
                }
            }
        }

        internal static string WslArgument(string value)
        {
            // This WSL CLI preserves redundant quotes in the distribution name.
            // Installation admits only this unambiguous, shell-free argv subset.
            if (!System.Text.RegularExpressions.Regex.IsMatch(value, "\\A[A-Za-z0-9_./-]+\\z"))
                throw new ArgumentException("Unsupported WSL installation argument.");
            return value;
        }

        private static void Log(HostSettings settings, string message)
        {
            lock (LogGate)
            {
                var path = Path.Combine(settings.LogDirectory, "host.log");
                if (File.Exists(path) && new FileInfo(path).Length > 1024 * 1024)
                {
                    var old = path + ".1";
                    if (File.Exists(old)) File.Delete(old);
                    File.Move(path, old);
                }
                File.AppendAllText(path, DateTimeOffset.Now.ToString("o") + " " + message + Environment.NewLine);
            }
        }

    }
}
