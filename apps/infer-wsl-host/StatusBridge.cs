// Read-only infer-runtime.status transport. Never accepts inference or operator commands.
using Microsoft.Win32.SafeHandles;
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Runtime.InteropServices;
using System.Runtime.Serialization.Json;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using System.Web.Script.Serialization;
using System.Xml;

namespace InferWslHost
{
    internal sealed class StatusBridge : IDisposable
    {
        private const int Limit = 256 * 1024;
        private readonly HostSettings settings;
        private readonly HttpClient http;
        private readonly string pipeName = "ir-" + Guid.NewGuid().ToString("N");
        private readonly string hostGeneration = Guid.NewGuid().ToString("N");
        private readonly SemaphoreSlim clients = new SemaphoreSlim(6);
        private sealed class PublishedIdentity
        {
            internal readonly string Source;
            internal readonly string Generation;
            internal PublishedIdentity(string source, string generation) { Source = source; Generation = generation; }
        }
        private volatile PublishedIdentity identity;
        private string reconciledSource;
        private NamedPipeServerStream listening;

        internal StatusBridge(HostSettings settings)
        {
            this.settings = settings;
            http = new HttpClient(new HttpClientHandler { UseProxy = false, AllowAutoRedirect = false });
            http.Timeout = TimeSpan.FromMilliseconds(1200);
            listening = CreatePipe(true);
        }

        internal async Task<Dictionary<string, object>> Snapshot()
        {
            PrivateFiles.Validate(settings.CredentialFile, false);
            var token = File.ReadAllText(settings.CredentialFile).Trim();
            if (!System.Text.RegularExpressions.Regex.IsMatch(token, "\\A[a-f0-9]{64}\\z")) throw new InvalidDataException();
            using (var cancellation = new CancellationTokenSource(1400))
            using (var request = new HttpRequestMessage(HttpMethod.Get, settings.Endpoint + "/infer/v1/observer/snapshot"))
            {
                request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
                using (var response = await http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation.Token))
                using (cancellation.Token.Register(() => response.Dispose()))
                {
                    response.EnsureSuccessStatusCode();
                    using (var stream = await response.Content.ReadAsStreamAsync())
                    using (var memory = new MemoryStream())
                    {
                        var buffer = new byte[8192];
                        int count;
                        while ((count = await stream.ReadAsync(buffer, 0, Math.Min(buffer.Length, Limit + 1 - (int)memory.Length), cancellation.Token)) != 0)
                        {
                            memory.Write(buffer, 0, count);
                            if (memory.Length >= Limit) throw new InvalidDataException();
                        }
                        var value = Json().Deserialize<Dictionary<string, object>>(new UTF8Encoding(false, true).GetString(memory.ToArray()));
                        if ((string)value["schema"] != "infer-runtime.status.snapshot" || (string)value["schema_version"] != "20260810.1")
                            throw new InvalidDataException();
                        var identity = (Dictionary<string, object>)value["service"];
                        if ((string)identity["kind"] != "infer-runtime" || (string)identity["instance_id"] != "local" ||
                            !System.Text.RegularExpressions.Regex.IsMatch((string)identity["generation"], "\\A[A-Za-z0-9][A-Za-z0-9._-]{0,127}\\z"))
                            throw new InvalidDataException();
                        return value;
                    }
                }
            }
        }

        internal async Task Reconcile()
        {
            var snapshot = await Within(Snapshot(), 1800);
            var identity = (Dictionary<string, object>)snapshot["service"];
            var observedGeneration = (string)identity["generation"];
            if (observedGeneration == reconciledSource) return;
            var next = "gen_" + hostGeneration + "_" + observedGeneration;
            if (next.Length > 96)
            {
                using (var hash = System.Security.Cryptography.SHA256.Create())
                    next = "gen_" + BitConverter.ToString(hash.ComputeHash(Encoding.UTF8.GetBytes(next))).Replace("-", "").ToLowerInvariant();
            }
            var manifest = new {
                schema = "infra.discovery.registration", schema_version = "20260812.1",
                service = new { kind = "infer-runtime", instance_id = "local", generation = next },
                offers = new object[] {
                    new { protocol = "infer-runtime.status", protocol_versions = new[] { "20260810.1" },
                        binding = "infra.local.windows-named-pipe", endpoint = @"\\.\pipe\infra-protocol\" + pipeName },
                    new { protocol = "infer-runtime.consumer-core", protocol_versions = new[] { "20260813.1" },
                        binding = "infer-runtime.http-loopback", endpoint = settings.Endpoint }
                }
            };
            // Response identity changes before publication: an in-flight old-generation
            // observation is rejected by Sentinel's mandatory identity/reread checks.
            this.identity = new PublishedIdentity(observedGeneration, next);
            var bytes = Encoding.UTF8.GetBytes(Json().Serialize(manifest) + "\n");
            foreach (var root in new[] { settings.DiscoveryRoot, settings.LegacyDiscoveryRoot })
                PrivateFiles.Publish(Path.Combine(root, "registrations", "infer-runtime--local.json"), bytes);
            reconciledSource = observedGeneration;
        }

        internal async Task Serve()
        {
            while (true)
            {
                var connection = listening;
                await connection.WaitForConnectionAsync();
                // A listening instance always exists while a client instance is closed.
                listening = CreatePipe(false);
                if (!clients.Wait(0)) { connection.Dispose(); continue; }
                ObserveClient(Handle(connection));
            }
        }

        private async Task Handle(NamedPipeServerStream pipe)
        {
            using (pipe)
            using (var deadline = new CancellationTokenSource(1900))
            using (deadline.Token.Register(() => pipe.Dispose()))
            {
                try
                {
                    AdmitClient(pipe);
                    byte[] output;
                    bool invalid = false;
                    try
                    {
                        var bytes = new byte[513];
                        int size = 0;
                        while (true)
                        {
                            int count = await pipe.ReadAsync(bytes, size, bytes.Length - size, deadline.Token);
                            if (count == 0) throw new InvalidDataException();
                            size += count;
                            int lf = Array.IndexOf(bytes, (byte)10, 0, size);
                            if (lf >= 0)
                            {
                                if (lf != size - 1 || size > 512) throw new InvalidDataException();
                                ValidateRequest(bytes, size - 1);
                                break;
                            }
                            if (size >= 512) throw new InvalidDataException();
                        }
                    }
                    catch (Exception error)
                    {
                        if (!(error is InvalidDataException || error is XmlException || error is System.Runtime.Serialization.SerializationException)) throw;
                        invalid = true;
                    }
                    if (invalid)
                    {
                        output = Error("invalid_request");
                        await pipe.WriteAsync(output, 0, output.Length, deadline.Token);
                        return;
                    }
                    try
                    {
                        var snapshot = await Within(Snapshot(), 1500);
                        var identity = (Dictionary<string, object>)snapshot["service"];
                        var published = this.identity;
                        if (published == null || (string)identity["generation"] != published.Source) throw new InvalidDataException();
                        identity["generation"] = published.Generation;
                        output = Encoding.UTF8.GetBytes(Json().Serialize(snapshot) + "\n");
                        if (output.Length > Limit) throw new InvalidDataException();
                    }
                    catch { output = Error("snapshot_unavailable"); }
                    await pipe.WriteAsync(output, 0, output.Length, deadline.Token);
                    await pipe.FlushAsync(deadline.Token);
                }
                catch (IOException) { }
                catch (ObjectDisposedException) { }
                catch (OperationCanceledException) { }
                catch (UnauthorizedAccessException) { }
                finally { clients.Release(); }
            }
        }

        internal static void ValidateRequest(byte[] bytes, int size)
        {
            // JSON's XML reader retains duplicate members; the set rejects them.
            using (var reader = JsonReaderWriterFactory.CreateJsonReader(bytes, 0, size, Encoding.UTF8, XmlDictionaryReaderQuotas.Max, null))
            {
                reader.MoveToContent();
                if (reader.GetAttribute("type") != "object") throw new InvalidDataException();
                reader.ReadStartElement("root");
                var seen = new HashSet<string>();
                while (reader.MoveToContent() == XmlNodeType.Element)
                {
                    var key = reader.LocalName;
                    if (!seen.Add(key) || reader.GetAttribute("type") != "string") throw new InvalidDataException();
                    var value = reader.ReadElementContentAsString();
                    if (!((key == "schema" && value == "infer-runtime.status.request") ||
                        (key == "schema_version" && value == "20260810.1") || (key == "operation" && value == "snapshot")))
                        throw new InvalidDataException();
                }
                reader.ReadEndElement();
                if (seen.Count != 3 || reader.MoveToContent() != XmlNodeType.None) throw new InvalidDataException();
            }
        }

        private static JavaScriptSerializer Json() { return new JavaScriptSerializer { MaxJsonLength = Limit, RecursionLimit = 64 }; }
        private static async void ObserveClient(Task task) { try { await task; } catch { /* Connection-local failure; no payload logging. */ } }
        private static byte[] Error(string code)
        {
            return Encoding.UTF8.GetBytes("{\"schema\":\"infer-runtime.status.error\",\"schema_version\":\"20260810.1\",\"error\":{\"code\":\"" + code + "\"}}\n");
        }

        private static async Task<T> Within<T>(Task<T> task, int milliseconds)
        {
            if (await Task.WhenAny(task, Task.Delay(milliseconds)) != task) throw new TimeoutException();
            return await task;
        }

        private NamedPipeServerStream CreatePipe(bool first)
        {
            var security = new RawSecurityDescriptor("O:" + PrivateFiles.User.Value + "G:" + PrivateFiles.User.Value +
                "D:P(A;;GA;;;" + PrivateFiles.User.Value + ")(A;;GA;;;SY)");
            var bytes = new byte[security.BinaryLength];
            security.GetBinaryForm(bytes, 0);
            var pinned = GCHandle.Alloc(bytes, GCHandleType.Pinned);
            try
            {
                var attributes = new SecurityAttributes { Length = Marshal.SizeOf(typeof(SecurityAttributes)), Descriptor = pinned.AddrOfPinnedObject() };
                // Duplex overlapped byte pipe; reject remote clients at the kernel boundary.
                var handle = CreateNamedPipe(@"\\.\pipe\infra-protocol\" + pipeName,
                    0x40000003u | (first ? 0x00080000u : 0u), 0x00000008, 8, Limit, 512, 2000, ref attributes);
                if (handle.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
                return new NamedPipeServerStream(PipeDirection.InOut, true, false, handle);
            }
            finally { pinned.Free(); }
        }

        private static void AdmitClient(NamedPipeServerStream pipe)
        {
            uint pid;
            if (!GetNamedPipeClientProcessId(pipe.SafePipeHandle, out pid)) throw new UnauthorizedAccessException();
            var process = OpenProcess(0x1000, false, pid); // PROCESS_QUERY_LIMITED_INFORMATION only
            if (process == IntPtr.Zero) throw new UnauthorizedAccessException();
            try
            {
                IntPtr token;
                if (!OpenProcessToken(process, 0x0008, out token)) throw new UnauthorizedAccessException();
                try
                {
                    using (var identity = new WindowsIdentity(token))
                        if (!PrivateFiles.User.Equals(identity.User)) throw new UnauthorizedAccessException();
                }
                finally { CloseHandle(token); }
            }
            finally { CloseHandle(process); }
        }

        public void Dispose() { if (listening != null) listening.Dispose(); http.Dispose(); }

        [StructLayout(LayoutKind.Sequential)]
        private struct SecurityAttributes { internal int Length; internal IntPtr Descriptor; internal int Inherit; }
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafePipeHandle CreateNamedPipe(string name, uint openMode, uint pipeMode, uint instances, int outSize, int inSize, uint timeout, ref SecurityAttributes attributes);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetNamedPipeClientProcessId(SafePipeHandle pipe, out uint pid);
        [DllImport("advapi32.dll", SetLastError = true)]
        private static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
        [DllImport("kernel32.dll")]
        private static extern bool CloseHandle(IntPtr handle);
    }
}
