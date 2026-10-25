// Ownership and publication of this host's private Windows install/discovery objects.
using System;
using System.Collections.Generic;
using System.IO;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;
using System.Web.Script.Serialization;

namespace InferWslHost
{
    internal sealed class HostSettings
    {
        public string Distribution { get; set; }
        public string LinuxInstall { get; set; }
        public string CredentialFile { get; set; }
        public string DiscoveryRoot { get; set; }
        public string LegacyDiscoveryRoot { get; set; }
        public string LogDirectory { get; set; }
        public string Endpoint { get; set; }

        internal static HostSettings Load(string path)
        {
            PrivateFiles.Validate(path, false);
            var settings = new JavaScriptSerializer().Deserialize<HostSettings>(File.ReadAllText(path));
            if (settings == null || String.IsNullOrWhiteSpace(settings.Distribution) ||
                String.IsNullOrWhiteSpace(settings.LinuxInstall) || !settings.LinuxInstall.StartsWith("/home/", StringComparison.Ordinal) ||
                settings.LinuxInstall.IndexOfAny(new[] { '"', '\r', '\n' }) >= 0)
                throw new InvalidDataException("Invalid installed host settings.");
            Program.WslArgument(settings.Distribution);
            Program.WslArgument(settings.LinuxInstall);
            foreach (var root in new[] { settings.DiscoveryRoot, settings.LegacyDiscoveryRoot, settings.LogDirectory })
                PrivateFiles.EnsureDirectory(root);
            PrivateFiles.Validate(settings.CredentialFile, false);
            var uri = new Uri(settings.Endpoint);
            System.Net.IPAddress address;
            if (uri.Scheme != "http" || !System.Net.IPAddress.TryParse(uri.Host, out address) ||
                !System.Net.IPAddress.IsLoopback(address) || uri.UserInfo != "" || uri.AbsolutePath != "/" ||
                uri.Query != "" || uri.Fragment != "") throw new InvalidDataException("Expected a numeric loopback origin.");
            return settings;
        }
    }

    internal static class PrivateFiles
    {
        internal static readonly SecurityIdentifier User = WindowsIdentity.GetCurrent().User;
        private static readonly SecurityIdentifier System = new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null);

        internal static void Validate(string path, bool directory)
        {
            var full = Path.GetFullPath(path);
            if (full.StartsWith(@"\\", StringComparison.Ordinal) || new DriveInfo(Path.GetPathRoot(full)).DriveType != DriveType.Fixed ||
                full.Substring(2).Contains(":")) throw new IOException("Private files require a local fixed drive.");
            for (var current = full; current != null; current = Path.GetDirectoryName(current))
                if ((Directory.Exists(current) || File.Exists(current)) && (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0)
                    throw new IOException("Private files cannot traverse reparse points.");
            FileSystemSecurity acl = directory ? (FileSystemSecurity)Directory.GetAccessControl(full) : File.GetAccessControl(full);
            if (!User.Equals(acl.GetOwner(typeof(SecurityIdentifier)))) throw new UnauthorizedAccessException();
            foreach (FileSystemAccessRule rule in acl.GetAccessRules(true, true, typeof(SecurityIdentifier)))
                if (rule.AccessControlType == AccessControlType.Allow && !User.Equals(rule.IdentityReference) && !System.Equals(rule.IdentityReference))
                    throw new UnauthorizedAccessException();
        }

        internal static void EnsureDirectory(string path)
        {
            if (Directory.Exists(path)) { Validate(path, true); return; }
            var acl = new DirectorySecurity();
            acl.SetOwner(User);
            acl.SetAccessRuleProtection(true, false);
            foreach (var sid in new[] { User, System })
                acl.AddAccessRule(new FileSystemAccessRule(sid, FileSystemRights.FullControl,
                    InheritanceFlags.ContainerInherit | InheritanceFlags.ObjectInherit, PropagationFlags.None, AccessControlType.Allow));
            Directory.CreateDirectory(path, acl);
            Validate(path, true);
        }

        internal static void Publish(string path, byte[] bytes)
        {
            EnsureDirectory(Path.GetDirectoryName(path));
            if (File.Exists(path))
            {
                Validate(path, false);
                if (Convert.ToBase64String(File.ReadAllBytes(path)) == Convert.ToBase64String(bytes)) return;
            }
            var temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            try
            {
                var acl = new FileSecurity();
                acl.SetOwner(User);
                acl.SetAccessRuleProtection(true, false);
                foreach (var sid in new[] { User, System })
                    acl.AddAccessRule(new FileSystemAccessRule(sid, FileSystemRights.FullControl, AccessControlType.Allow));
                using (var stream = new FileStream(temporary, FileMode.CreateNew, FileSystemRights.Write,
                    FileShare.None, 4096, FileOptions.WriteThrough, acl)) stream.Write(bytes, 0, bytes.Length);
                if (File.Exists(path)) File.Replace(temporary, path, null);
                else File.Move(temporary, path);
            }
            finally { if (File.Exists(temporary)) File.Delete(temporary); }
        }
    }
}
