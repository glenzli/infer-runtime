// Exercise production framing, native pipe admission, publication and upstream boundaries.
using System;
using System.Collections.Generic;
using System.IO;
using System.IO.Pipes;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using System.Web.Script.Serialization;

namespace InferWslHost
{
    internal static class StatusBridgeTests
    {
        private static readonly string Request = "{\"schema\":\"infer-runtime.status.request\",\"schema_version\":\"20260810.1\",\"operation\":\"snapshot\"}";
        private static int Main()
        {
            var root = Path.Combine(Path.GetTempPath(), "infer-host-tests-" + Guid.NewGuid().ToString("N"));
            PrivateFiles.EnsureDirectory(root);
            try { Run(root).GetAwaiter().GetResult(); Console.WriteLine("Status bridge boundary checks passed."); return 0; }
            catch (Exception error) { Console.Error.WriteLine(error); return 1; }
            finally { Directory.Delete(root, true); }
        }

        private static async Task Run(string root)
        {
            var broad = Path.Combine(root, "broad");
            PrivateFiles.EnsureDirectory(broad);
            var acl = Directory.GetAccessControl(broad);
            acl.AddAccessRule(new System.Security.AccessControl.FileSystemAccessRule(
                new System.Security.Principal.SecurityIdentifier(System.Security.Principal.WellKnownSidType.WorldSid, null),
                System.Security.AccessControl.FileSystemRights.Read, System.Security.AccessControl.AccessControlType.Allow));
            Directory.SetAccessControl(broad, acl);
            bool broadRejected = false;
            try { PrivateFiles.Validate(broad, true); } catch (UnauthorizedAccessException) { broadRejected = true; }
            Require(broadRejected, "A shared Discovery directory was accepted.");
            Require(Program.WslArgument("Ubuntu-24.04") == "Ubuntu-24.04", "Distribution quoting changed.");
            foreach (var unsafeArgument in new[] { "Ubuntu 24.04", "Ubuntu\"", "/home/user name/install" })
            {
                bool rejected = false;
                try { Program.WslArgument(unsafeArgument); } catch (ArgumentException) { rejected = true; }
                Require(rejected, "Ambiguous WSL command line was admitted.");
            }
            Validate(" { \"operation\" : \"snapshot\", \"schema_version\":\"20260810.1\", \"schema\":\"infer-runtime.status.request\" } ");
            foreach (var invalid in new[] {
                Request.Replace("\"operation\":\"snapshot\"", "\"operation\":\"snapshot\",\"operation\":\"snapshot\""),
                Request.Replace("snapshot", "restart"), Request.Replace("}", ",\"extra\":true}"),
                "[]", Request + Request })
            {
                bool rejected = false;
                try { Validate(invalid); } catch { rejected = true; }
                Require(rejected, "Invalid request accepted.");
            }
            using (var provider = new FakeProvider())
            {
                var credential = Path.Combine(root, "observer.token");
                PrivateFiles.Publish(credential, Encoding.UTF8.GetBytes(new string('a', 64)));
                var settings = new HostSettings { Distribution = "Ubuntu-24.04", LinuxInstall = "/home/test/.local/lib/infer-runtime",
                    CredentialFile = credential, DiscoveryRoot = Path.Combine(root, "canonical"),
                    LegacyDiscoveryRoot = Path.Combine(root, "legacy"), Endpoint = provider.Endpoint };
                PrivateFiles.EnsureDirectory(settings.DiscoveryRoot);
                PrivateFiles.EnsureDirectory(settings.LegacyDiscoveryRoot);
                using (var bridge = new StatusBridge(settings))
                {
                    var serving = bridge.Serve();
                    await bridge.Reconcile();
                    var file = Path.Combine(settings.DiscoveryRoot, "registrations", "infer-runtime--local.json");
                    PrivateFiles.Validate(file, false);
                    var original = File.ReadAllText(file);
                    var manifest = new JavaScriptSerializer().Deserialize<Dictionary<string, object>>(original);
                    var service = (Dictionary<string, object>)manifest["service"];
                    var generation = (string)service["generation"];
                    var offers = (System.Collections.ArrayList)manifest["offers"];
                    Require(offers.Count == 3, "Expected status, Consumer Core and typed artifact-lease offers.");
                    var raw = (Dictionary<string, object>)offers[2];
                    Require((string)raw["binding"] == "infer-runtime.wsl-stdio-scm-rights" &&
                        (string)raw["endpoint"] == "wsl-stdio://Ubuntu-24.04/home/test/.local/lib/infer-runtime/current/raw_foundation_wsl_bridge.py",
                        "RAW binding must reference installed code, never a source or artifact path.");
                    var pipe = ((string)((Dictionary<string, object>)offers[0])["endpoint"]).Substring(@"\\.\pipe\".Length);
                    var modified = File.GetLastWriteTimeUtc(file);
                    await bridge.Reconcile();
                    Require(File.GetLastWriteTimeUtc(file) == modified, "Unchanged registration was rewritten.");
                    Require(original == File.ReadAllText(Path.Combine(settings.LegacyDiscoveryRoot, "registrations", "infer-runtime--local.json")), "Compatibility mirror diverged.");
                    var response = await Exchange(pipe, Request + "\n");
                    var snapshot = new JavaScriptSerializer().Deserialize<Dictionary<string, object>>(response);
                    Require((string)((Dictionary<string, object>)snapshot["service"])["generation"] == generation, "Snapshot identity differs from registration.");
                    Require(provider.Reads >= 3 && provider.BadRequests == 0, "Bridge crossed the upstream observer boundary.");
                    provider.Padding = new string('x', 200 * 1024);
                    Require((await Exchange(pipe, Request + "\n")).Length > 200 * 1024, "Large response lost bytes before EOF.");
                    provider.Padding = "";
                    var before = provider.Reads;
                    foreach (var invalid in new[] { Request.Replace("snapshot", "restart") + "\n", Request + "\ntrailing", new string('x', 513) })
                        Require((await Exchange(pipe, invalid)).Contains("invalid_request"), "Malformed native request was admitted.");
                    Require(provider.Reads == before, "Rejected commands reached the upstream.");
                    using (var idle = new NamedPipeClientStream(".", pipe, PipeDirection.InOut, PipeOptions.Asynchronous))
                    {
                        await idle.ConnectAsync(1000);
                        var started = DateTime.UtcNow;
                        var count = await idle.ReadAsync(new byte[1], 0, 1);
                        Require(count == 0 && DateTime.UtcNow - started < TimeSpan.FromSeconds(3), "Idle connection did not close within its deadline.");
                    }
                    Require((await Exchange(pipe, Request + "\n")).Contains("infer-runtime.status.snapshot"), "Idle client broke subsequent requests.");
                    provider.Generation = "gen_second";
                    Require((await Exchange(pipe, Request + "\n")).Contains("snapshot_unavailable"), "Stale source generation was forwarded.");
                    await bridge.Reconcile();
                    Require(File.ReadAllText(file) != original, "Daemon restart did not advance discovery.");
                    Require((await Exchange(pipe, Request + "\n")).Contains("infer-runtime.status.snapshot"), "Restarted source did not recover.");
                    provider.HoldBody = true;
                    var time = DateTime.UtcNow;
                    Require((await Exchange(pipe, Request + "\n")).Contains("snapshot_unavailable"), "Stalled upstream did not fail closed.");
                    Require(DateTime.UtcNow - time < TimeSpan.FromSeconds(2), "Stalled upstream exceeded the consumer deadline.");
                    GC.KeepAlive(serving);
                }
            }
        }

        private static void Validate(string value) { var bytes = Encoding.UTF8.GetBytes(value); StatusBridge.ValidateRequest(bytes, bytes.Length); }
        private static void Require(bool condition, string message) { if (!condition) throw new Exception(message); }
        private static async Task<string> Exchange(string name, string request)
        {
            using (var pipe = new NamedPipeClientStream(".", name, PipeDirection.InOut, PipeOptions.Asynchronous))
            using (var deadline = new CancellationTokenSource(2500))
            using (deadline.Token.Register(() => pipe.Dispose()))
            {
                await pipe.ConnectAsync(1000);
                var bytes = Encoding.UTF8.GetBytes(request);
                await pipe.WriteAsync(bytes, 0, bytes.Length, deadline.Token);
                using (var output = new MemoryStream())
                {
                    var buffer = new byte[8192];
                    int count;
                    while ((count = await pipe.ReadAsync(buffer, 0, buffer.Length, deadline.Token)) != 0) output.Write(buffer, 0, count);
                    var value = Encoding.UTF8.GetString(output.ToArray());
                    Require(value.EndsWith("\n") && value.IndexOf('\n') == value.Length - 1, "Response framing/EOF violated.");
                    return value;
                }
            }
        }

        private sealed class FakeProvider : IDisposable
        {
            private readonly TcpListener listener = new TcpListener(IPAddress.Loopback, 0);
            internal string Endpoint;
            internal string Generation = "gen_first";
            internal string Padding = "";
            internal volatile bool HoldBody;
            internal int Reads;
            internal int BadRequests;
            internal FakeProvider()
            {
                listener.Start(); Endpoint = "http://127.0.0.1:" + ((IPEndPoint)listener.LocalEndpoint).Port;
                Accept();
            }
            private async void Accept()
            {
                try { while (true) Reply(await listener.AcceptTcpClientAsync()); }
                catch (ObjectDisposedException) { }
                catch (SocketException) { }
            }
            private async void Reply(TcpClient client)
            {
                try
                {
                    using (client)
                    using (var stream = client.GetStream())
                    {
                        var reader = new StreamReader(stream, Encoding.ASCII, false, 1024, true);
                        var first = await reader.ReadLineAsync();
                        string line; bool authorized = false;
                        while (!String.IsNullOrEmpty(line = await reader.ReadLineAsync()))
                            if (line == "Authorization: Bearer " + new string('a', 64)) authorized = true;
                        if (first != "GET /infer/v1/observer/snapshot HTTP/1.1" || !authorized) Interlocked.Increment(ref BadRequests);
                        Interlocked.Increment(ref Reads);
                        var body = "{\"schema\":\"infer-runtime.status.snapshot\",\"schema_version\":\"20260810.1\",\"service\":{\"kind\":\"infer-runtime\",\"instance_id\":\"local\",\"generation\":\"" + Generation + "\"},\"padding\":\"" + Padding + "\"}";
                        var header = Encoding.ASCII.GetBytes("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: " + body.Length + "\r\n\r\n");
                        await stream.WriteAsync(header, 0, header.Length);
                        if (HoldBody) await Task.Delay(2200);
                        var bytes = Encoding.UTF8.GetBytes(body);
                        await stream.WriteAsync(bytes, 0, bytes.Length);
                    }
                }
                catch (IOException) { }
                catch (ObjectDisposedException) { }
            }
            public void Dispose() { listener.Stop(); }
        }
    }
}
