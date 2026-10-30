<#
.SYNOPSIS
    End-to-end smoke test of TedCat.exe on a real (or CI) Windows desktop.

.DESCRIPTION
    Starts TedCat.exe, waits for its input hook, then drives it with SendInput
    (injected events without TedCat's "TCAT" dwExtraInfo marker count as real
    input by design) against a small WinForms test window hosted by this
    script, and checks:

      baseline      SendInput reaches the test window before TedCat runs
                    (proves the desktop is interactive; otherwise every
                    gesture step is reported SKIPPED with the evidence).
      start         TedCat starts and reports "input hook status: Active".
      replay-click  a quick click on a counter button counts exactly once.
      replay-double a quick double click arrives as a double click.
      replay-drag   a quick drag still moves a draggable knob, with
                    down / move / up delivered in order.
      screenshot    hold 600 ms, drag, release: the overlay shows while
                    selecting and goes away; the clipboard holds an image of
                    the selected size and content (CF_DIBV5 via
                    Clipboard.GetImage and the registered "PNG" format), and
                    the press never reached the window underneath.
      escape-cancel Escape during a selection closes the overlay, takes no
                    screenshot and never delivers the press.
      recording     Shift + hold, drag >= 400x300, release; no yellow capture
                    border; click the recording pill (TedCat.Hud) to stop; an
                    .mp4 appears in Videos\TedCat; with ffprobe: H.264,
                    expected even size, duration close to the recording time.
      kill-switch   Escape x3 turns the gesture off: a later hold + drag
                    passes straight through (no overlay, no screenshot) and
                    a click still works.
      after-exit    after TedCat is killed, clicks still work.
      crash-mid-gesture
                    TedCat killed while a press is held (undecided) and in
                    the middle of a selection: no overlay is left behind and
                    the next click works.

    The start step also checks the tray icon and that a second TedCat exits
    at once (single instance).

    The test window tags every button message with GetMessageExtraInfo():
    "TCAT" means TedCat held the event and replayed it, "0x0" means it came
    straight from this script, which tells hold-and-replay from pass-through.

    TedCat is always stopped at the end, its log printed, and the script exits
    non-zero if any step FAILED. Artifacts (log, recordings, desktop
    screenshots of failures, summary.json) go to -ArtifactsDir.

    Runs on Windows PowerShell 5.1 (STA; it re-launches itself with -STA if needed).

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Exe target\release\TedCat.exe
#>
[CmdletBinding()]
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\TedCat.exe'),
    [string]$ArtifactsDir = (Join-Path (Get-Location) 'smoke-artifacts'),
    [int]$RecordSeconds = 3
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

if ([System.Threading.Thread]::CurrentThread.GetApartmentState() -ne [System.Threading.ApartmentState]::STA) {
    Write-Host 'Not in a single-threaded apartment; re-launching with -STA.'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -STA -File $PSCommandPath -Exe $Exe -ArtifactsDir $ArtifactsDir -RecordSeconds $RecordSeconds
    exit $LASTEXITCODE
}

$Exe = [System.IO.Path]::GetFullPath($Exe)
$ArtifactsDir = [System.IO.Path]::GetFullPath($ArtifactsDir)
New-Item -ItemType Directory -Force -Path $ArtifactsDir | Out-Null

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# ---------------------------------------------------------------------------
# Native helpers (C# 5: compiled by Windows PowerShell 5.1's Add-Type).

$helpers = @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Windows.Forms;

namespace TedCatSmoke
{
    public static class Native
    {
        [StructLayout(LayoutKind.Sequential)]
        public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
        [StructLayout(LayoutKind.Sequential)]
        public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
        [StructLayout(LayoutKind.Explicit)]
        public struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
        [StructLayout(LayoutKind.Sequential)]
        public struct INPUT { public uint type; public InputUnion u; }
        [StructLayout(LayoutKind.Sequential)]
        public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
        [StructLayout(LayoutKind.Sequential)]
        public struct POINT { public int X; public int Y; }

        [DllImport("user32.dll", SetLastError = true)]
        public static extern uint SendInput(uint count, [In, MarshalAs(UnmanagedType.LPArray)] INPUT[] inputs, int size);
        [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
        [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT point);
        [DllImport("winmm.dll")] public static extern uint timeBeginPeriod(uint period);

        public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lparam);
        [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc proc, IntPtr lparam);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr hwnd, StringBuilder name, int max);
        [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
        [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
        [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT point);
        [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] public static extern IntPtr GetMessageExtraInfo();

        [StructLayout(LayoutKind.Sequential)]
        public struct NOTIFYICONIDENTIFIER { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
        [DllImport("shell32.dll")] public static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER identifier, out RECT rect);

        [DllImport("user32.dll", SetLastError = true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
        [DllImport("user32.dll", SetLastError = true)] public static extern bool CloseDesktop(IntPtr desktop);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern bool GetUserObjectInformation(IntPtr obj, int index, StringBuilder info, int length, out int needed);
        [DllImport("kernel32.dll")] public static extern uint WTSGetActiveConsoleSessionId();
        [DllImport("user32.dll", SetLastError = true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
        [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
        [DllImport("user32.dll")] public static extern uint GetDpiForSystem();

        [DllImport("user32.dll", SetLastError = true)] public static extern bool OpenClipboard(IntPtr owner);
        [DllImport("user32.dll", SetLastError = true)] public static extern bool CloseClipboard();
        [DllImport("user32.dll", SetLastError = true)] public static extern bool EmptyClipboard();
        [DllImport("user32.dll")] public static extern uint EnumClipboardFormats(uint format);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClipboardFormatName(uint format, StringBuilder name, int max);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern uint RegisterClipboardFormat(string name);
        [DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint format);
        [DllImport("user32.dll")] public static extern IntPtr GetClipboardOwner();
        [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr mem);
        [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr mem);
        [DllImport("kernel32.dll")] public static extern UIntPtr GlobalSize(IntPtr mem);
    }

    /// Injected input. dwExtraInfo is 0, never TedCat's "TCAT" marker, so
    /// TedCat's hook treats these events like real input.
    public static class Input
    {
        public static string LastError = "";
        public static int Sent;

        static Input() { Native.timeBeginPeriod(1); }

        static int Normalize(int value, int origin, int extent)
        {
            if (extent <= 1) return 0;
            long offset = Math.Max(0L, Math.Min((long)extent - 1, (long)value - origin));
            return (int)Math.Min(65535L, (offset * 65536 + 32768) / extent);
        }

        static Native.INPUT Mouse(int x, int y, uint buttons)
        {
            int left = Native.GetSystemMetrics(76), top = Native.GetSystemMetrics(77);
            int width = Native.GetSystemMetrics(78), height = Native.GetSystemMetrics(79);
            Native.INPUT input = new Native.INPUT();
            input.type = 0;
            input.u.mi.dx = Normalize(x, left, width);
            input.u.mi.dy = Normalize(y, top, height);
            // MOVE | ABSOLUTE | VIRTUALDESK, plus the button flags.
            input.u.mi.dwFlags = buttons | 0x0001 | 0x8000 | 0x4000;
            input.u.mi.dwExtraInfo = IntPtr.Zero;
            return input;
        }

        static bool Send(Native.INPUT input)
        {
            Native.INPUT[] inputs = new Native.INPUT[] { input };
            uint inserted = Native.SendInput(1, inputs, Marshal.SizeOf(typeof(Native.INPUT)));
            if (inserted != 1)
            {
                LastError = "SendInput inserted " + inserted + " of 1 events, error " + Marshal.GetLastWin32Error();
                return false;
            }
            Sent++;
            return true;
        }

        static bool Pause(int ms) { if (ms > 0) Thread.Sleep(ms); return true; }

        public static bool Move(int x, int y) { return Send(Mouse(x, y, 0)); }
        public static bool Down(int x, int y) { return Send(Mouse(x, y, 0x0002)); }
        public static bool Up(int x, int y) { return Send(Mouse(x, y, 0x0004)); }

        public static bool Key(ushort vk, ushort scan, bool up)
        {
            Native.INPUT input = new Native.INPUT();
            input.type = 1;
            input.u.ki.wVk = vk;
            input.u.ki.wScan = scan;
            input.u.ki.dwFlags = up ? 0x0002u : 0u;
            input.u.ki.dwExtraInfo = IntPtr.Zero;
            return Send(input);
        }

        public static bool Tap(ushort vk, ushort scan, int holdMs)
        {
            return Key(vk, scan, false) && Pause(holdMs) && Key(vk, scan, true);
        }

        /// Move there, press, wait holdMs, release.
        public static bool Click(int x, int y, int holdMs)
        {
            return Move(x, y) && Pause(40) && Down(x, y) && Pause(holdMs) && Up(x, y);
        }

        public static bool DoubleClick(int x, int y, int holdMs, int gapMs)
        {
            return Move(x, y) && Pause(40)
                && Down(x, y) && Pause(holdMs) && Up(x, y) && Pause(gapMs)
                && Down(x, y) && Pause(holdMs) && Up(x, y);
        }

        /// Press at (x1, y1), hold still for holdMs, move in `steps` steps to (x2, y2), release.
        public static bool Drag(int x1, int y1, int x2, int y2, int holdMs, int steps, int stepMs)
        {
            if (!(Move(x1, y1) && Pause(60) && Down(x1, y1) && Pause(holdMs))) return false;
            for (int i = 1; i <= steps; i++)
            {
                int x = x1 + (x2 - x1) * i / steps;
                int y = y1 + (y2 - y1) * i / steps;
                if (!(Move(x, y) && Pause(stepMs))) return false;
            }
            return Pause(60) && Up(x2, y2);
        }

        /// Moves from (x1, y1) to (x2, y2) in `steps` steps, `stepMs` apart.
        public static bool MovePath(int x1, int y1, int x2, int y2, int steps, int stepMs)
        {
            for (int i = 1; i <= steps; i++)
            {
                if (!(Move(x1 + (x2 - x1) * i / steps, y1 + (y2 - y1) * i / steps) && Pause(stepMs))) return false;
            }
            return true;
        }

        public static string Cursor()
        {
            Native.POINT p;
            if (!Native.GetCursorPos(out p)) return "unknown (error " + Marshal.GetLastWin32Error() + ")";
            return p.X + "," + p.Y;
        }
    }

    public static class Clip
    {
        static bool Open()
        {
            for (int i = 0; i < 50; i++)
            {
                // A NULL owner: emptying never makes this process the owner, so
                // TedCat's EmptyClipboard never has to wait for this script.
                if (Native.OpenClipboard(IntPtr.Zero)) return true;
                Thread.Sleep(20);
            }
            return false;
        }

        public static bool Clear()
        {
            if (!Open()) return false;
            try { return Native.EmptyClipboard(); }
            finally { Native.CloseClipboard(); }
        }

        static string FormatName(uint format)
        {
            switch (format)
            {
                case 1: return "CF_TEXT";
                case 2: return "CF_BITMAP";
                case 7: return "CF_OEMTEXT";
                case 8: return "CF_DIB";
                case 13: return "CF_UNICODETEXT";
                case 16: return "CF_LOCALE";
                case 17: return "CF_DIBV5";
            }
            StringBuilder name = new StringBuilder(256);
            if (Native.GetClipboardFormatName(format, name, name.Capacity) > 0) return name.ToString();
            return "#" + format;
        }

        public static string[] Formats()
        {
            List<string> names = new List<string>();
            if (!Open()) return new string[] { "(clipboard busy)" };
            try
            {
                uint format = 0;
                while ((format = Native.EnumClipboardFormats(format)) != 0) names.Add(FormatName(format));
            }
            finally { Native.CloseClipboard(); }
            return names.ToArray();
        }

        public static byte[] Data(string registeredName)
        {
            uint format = Native.RegisterClipboardFormat(registeredName);
            if (format == 0 || !Open()) return null;
            try
            {
                IntPtr handle = Native.GetClipboardData(format);
                if (handle == IntPtr.Zero) return null;
                IntPtr ptr = Native.GlobalLock(handle);
                if (ptr == IntPtr.Zero) return null;
                try
                {
                    int size = (int)Native.GlobalSize(handle).ToUInt64();
                    byte[] bytes = new byte[size];
                    Marshal.Copy(ptr, bytes, 0, size);
                    return bytes;
                }
                finally { Native.GlobalUnlock(handle); }
            }
            finally { Native.CloseClipboard(); }
        }

        /// "WxH" of the PNG on the clipboard, or null.
        public static string PngSize()
        {
            byte[] bytes = Data("PNG");
            if (bytes == null) return null;
            using (MemoryStream stream = new MemoryStream(bytes))
            using (Image image = Image.FromStream(stream))
            {
                return image.Width + "x" + image.Height;
            }
        }

        public static uint OwnerPid()
        {
            IntPtr owner = Native.GetClipboardOwner();
            if (owner == IntPtr.Zero) return 0;
            uint pid;
            Native.GetWindowThreadProcessId(owner, out pid);
            return pid;
        }
    }

    public static class Win
    {
        public static string InitDpi()
        {
            try
            {
                // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, like TedCat.
                if (Native.SetProcessDpiAwarenessContext(new IntPtr(-4))) return "per-monitor v2";
                return "SetProcessDpiAwarenessContext failed (error " + Marshal.GetLastWin32Error() + ")";
            }
            catch (EntryPointNotFoundException)
            {
                return Native.SetProcessDPIAware() ? "system aware" : "unaware";
            }
        }

        public static string InputDesktop()
        {
            IntPtr desktop = Native.OpenInputDesktop(0, false, 0x0001);
            if (desktop == IntPtr.Zero) return "none (OpenInputDesktop error " + Marshal.GetLastWin32Error() + ")";
            try
            {
                StringBuilder name = new StringBuilder(256);
                int needed;
                if (!Native.GetUserObjectInformation(desktop, 2, name, name.Capacity * 2, out needed)) return "unnamed";
                return name.ToString();
            }
            finally { Native.CloseDesktop(desktop); }
        }

        static string ClassOf(IntPtr hwnd)
        {
            StringBuilder name = new StringBuilder(256);
            Native.GetClassName(hwnd, name, name.Capacity);
            return name.ToString();
        }

        /// Every top-level window whose class starts with `prefix`: "class visible=.. rect=l,t,r,b pid=..".
        public static string[] Windows(string prefix)
        {
            List<string> found = new List<string>();
            Native.EnumWindows(delegate(IntPtr hwnd, IntPtr unused)
            {
                string cls = ClassOf(hwnd);
                if (cls.StartsWith(prefix, StringComparison.Ordinal))
                {
                    Native.RECT r;
                    Native.GetWindowRect(hwnd, out r);
                    uint pid;
                    Native.GetWindowThreadProcessId(hwnd, out pid);
                    found.Add(cls + " visible=" + Native.IsWindowVisible(hwnd) + " rect=" + r.Left + "," + r.Top + "," + r.Right + "," + r.Bottom + " pid=" + pid);
                }
                return true;
            }, IntPtr.Zero);
            return found.ToArray();
        }

        /// Rectangle (l, t, r, b) of the first visible top-level window of class `cls`, or null.
        public static int[] FindVisible(string cls)
        {
            int[] result = null;
            Native.EnumWindows(delegate(IntPtr hwnd, IntPtr unused)
            {
                if (ClassOf(hwnd) == cls && Native.IsWindowVisible(hwnd))
                {
                    Native.RECT r;
                    Native.GetWindowRect(hwnd, out r);
                    result = new int[] { r.Left, r.Top, r.Right, r.Bottom };
                    return false;
                }
                return true;
            }, IntPtr.Zero);
            return result;
        }

        /// Handle of the first top-level window of class `cls` (visible or not).
        public static IntPtr Find(string cls)
        {
            IntPtr result = IntPtr.Zero;
            Native.EnumWindows(delegate(IntPtr hwnd, IntPtr unused)
            {
                if (ClassOf(hwnd) == cls) { result = hwnd; return false; }
                return true;
            }, IntPtr.Zero);
            return result;
        }

        /// Where the shell shows tray icon `id` of `owner`, or why it cannot say.
        public static string TrayIcon(IntPtr owner, uint id)
        {
            Native.NOTIFYICONIDENTIFIER identifier = new Native.NOTIFYICONIDENTIFIER();
            identifier.cbSize = (uint)Marshal.SizeOf(typeof(Native.NOTIFYICONIDENTIFIER));
            identifier.hWnd = owner;
            identifier.uID = id;
            Native.RECT r;
            int hr = Native.Shell_NotifyIconGetRect(ref identifier, out r);
            if (hr != 0) return "error 0x" + hr.ToString("X8");
            return "rect=" + r.Left + "," + r.Top + "," + r.Right + "," + r.Bottom;
        }

        static bool Yellowish(Color c) { return c.R >= 170 && c.G >= 150 && c.B <= 110; }

        /// How many of the four screen edges (sampled at their midpoints, two
        /// pixels deep) show Windows.Graphics.Capture's yellow border, and the colours.
        public static string CaptureBorder(out int yellowEdges)
        {
            yellowEdges = 0;
            Rectangle s = Screen.PrimaryScreen.Bounds;
            Point[] points = new Point[] {
                new Point(s.Left, s.Top + s.Height / 2), new Point(s.Right - 1, s.Top + s.Height / 2),
                new Point(s.Left + s.Width / 2, s.Top), new Point(s.Left + s.Width / 2, s.Bottom - 1) };
            string[] names = new string[] { "left", "right", "top", "bottom" };
            StringBuilder text = new StringBuilder();
            using (Bitmap pixel = new Bitmap(1, 1))
            using (Graphics g = Graphics.FromImage(pixel))
            {
                for (int i = 0; i < points.Length; i++)
                {
                    g.CopyFromScreen(points[i].X, points[i].Y, 0, 0, new Size(1, 1));
                    Color c = pixel.GetPixel(0, 0);
                    if (Yellowish(c)) yellowEdges++;
                    text.Append(names[i] + "=" + c.R + "," + c.G + "," + c.B + " ");
                }
            }
            return text.ToString().Trim();
        }

        public static string WindowAt(int x, int y)
        {
            Native.POINT p; p.X = x; p.Y = y;
            IntPtr hwnd = Native.WindowFromPoint(p);
            if (hwnd == IntPtr.Zero) return "(none)";
            uint pid;
            Native.GetWindowThreadProcessId(hwnd, out pid);
            return ClassOf(hwnd) + " pid=" + pid;
        }

        public static string Foreground()
        {
            IntPtr hwnd = Native.GetForegroundWindow();
            if (hwnd == IntPtr.Zero) return "(none)";
            uint pid;
            Native.GetWindowThreadProcessId(hwnd, out pid);
            return ClassOf(hwnd) + " pid=" + pid;
        }

        /// Saves the whole virtual screen as PNG; returns null or the error.
        public static string Shot(string path)
        {
            try
            {
                Rectangle screen = SystemInformation.VirtualScreen;
                using (Bitmap bitmap = new Bitmap(screen.Width, screen.Height))
                {
                    using (Graphics g = Graphics.FromImage(bitmap)) g.CopyFromScreen(screen.Left, screen.Top, 0, 0, screen.Size);
                    bitmap.Save(path, ImageFormat.Png);
                }
                return null;
            }
            catch (Exception e) { return e.Message; }
        }

        public static string ReadShared(string path)
        {
            using (FileStream stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete))
            using (StreamReader reader = new StreamReader(stream, Encoding.UTF8))
            {
                return reader.ReadToEnd();
            }
        }
    }

    /// A panel that logs raw left-button messages (and moves with the button
    /// down) in screen coordinates; `Draggable` makes it follow a drag.
    public class EventPanel : Panel
    {
        public bool Draggable;
        bool dragging;
        Point startCursor;
        Point startLocation;

        static string MessageName(int msg)
        {
            switch (msg)
            {
                case 0x200: return "MOVE";
                case 0x201: return "DOWN";
                case 0x202: return "UP";
                case 0x203: return "DBLCLK";
            }
            return "0x" + msg.ToString("X");
        }

        protected override void WndProc(ref Message m)
        {
            int msg = m.Msg;
            if (msg >= 0x200 && msg <= 0x203)
            {
                long lp = m.LParam.ToInt64();
                Point screen = PointToScreen(new Point((short)(lp & 0xFFFF), (short)((lp >> 16) & 0xFFFF)));
                bool left = (m.WParam.ToInt64() & 1) != 0;
                if (msg != 0x200 || left) Ui.Log(Name, MessageName(msg), screen, Native.GetMessageExtraInfo());
                if (Draggable)
                {
                    if (msg == 0x201 || msg == 0x203)
                    {
                        dragging = true;
                        startCursor = screen;
                        startLocation = Location;
                        Capture = true;
                    }
                    else if (dragging && ((msg == 0x200 && left) || msg == 0x202))
                    {
                        Location = new Point(startLocation.X + screen.X - startCursor.X, startLocation.Y + screen.Y - startCursor.Y);
                        if (msg == 0x202) { dragging = false; Capture = false; }
                    }
                }
            }
            base.WndProc(ref m);
        }
    }

    /// A button that also logs its raw left-button messages, like EventPanel.
    public class CountingButton : Button
    {
        protected override void WndProc(ref Message m)
        {
            if (m.Msg == 0x201 || m.Msg == 0x202 || m.Msg == 0x203)
            {
                long lp = m.LParam.ToInt64();
                Point screen = PointToScreen(new Point((short)(lp & 0xFFFF), (short)((lp >> 16) & 0xFFFF)));
                Ui.Log(Name, m.Msg == 0x201 ? "DOWN" : m.Msg == 0x202 ? "UP" : "DBLCLK", screen, Native.GetMessageExtraInfo());
            }
            base.WndProc(ref m);
        }
    }

    public class SmokeForm : Form
    {
        public CountingButton Counter;
        public EventPanel Pad;
        public Panel DragArea;
        public EventPanel Knob;
        public EventPanel Canvas;
        public Label Ticker;
        public Point KnobHome = new Point(10, 20);
        System.Windows.Forms.Timer timer;

        public static readonly Color CanvasColor = Color.FromArgb(40, 160, 90);

        public SmokeForm(Rectangle bounds)
        {
            Text = "TedCat smoke test";
            StartPosition = FormStartPosition.Manual;
            FormBorderStyle = FormBorderStyle.FixedSingle;
            MaximizeBox = false;
            MinimizeBox = false;
            TopMost = true;
            Bounds = bounds;
            BackColor = Color.WhiteSmoke;
            int w = ClientSize.Width, h = ClientSize.Height;

            Counter = new CountingButton();
            Counter.Name = "Counter";
            Counter.Text = "Clicks: 0";
            Counter.SetBounds(10, 10, 200, 60);
            Counter.Click += delegate
            {
                int n = Interlocked.Increment(ref Ui.Clicks);
                Counter.Text = "Clicks: " + n;
                Ui.Log("Counter", "Click", Control.MousePosition);
            };

            Pad = new EventPanel();
            Pad.Name = "Pad";
            Pad.BackColor = Color.Khaki;
            Pad.SetBounds(220, 10, 200, 60);
            Pad.MouseDoubleClick += delegate(object sender, MouseEventArgs e) { Ui.Log("Pad", "MouseDoubleClick", Pad.PointToScreen(e.Location)); };

            DragArea = new Panel();
            DragArea.Name = "DragArea";
            DragArea.BackColor = Color.LightSteelBlue;
            DragArea.SetBounds(10, 80, w - 20, 100);

            Knob = new EventPanel();
            Knob.Name = "Knob";
            Knob.Draggable = true;
            Knob.BackColor = Color.OrangeRed;
            Knob.SetBounds(KnobHome.X, KnobHome.Y, 60, 60);
            DragArea.Controls.Add(Knob);

            Canvas = new EventPanel();
            Canvas.Name = "Canvas";
            Canvas.BackColor = CanvasColor;
            Canvas.SetBounds(10, 190, w - 20, h - 200);

            // Changes every 100 ms so the recording has moving pixels.
            Ticker = new Label();
            Ticker.Name = "Ticker";
            Ticker.AutoSize = false;
            Ticker.SetBounds(60, 60, 170, 32);
            Ticker.BackColor = Color.Black;
            Ticker.ForeColor = Color.White;
            Ticker.Font = new Font(FontFamily.GenericMonospace, 14f, FontStyle.Bold);
            Canvas.Controls.Add(Ticker);
            timer = new System.Windows.Forms.Timer();
            timer.Interval = 100;
            timer.Tick += delegate { Ticker.Text = DateTime.Now.ToString("HH:mm:ss.f"); };
            timer.Start();

            Controls.Add(Counter);
            Controls.Add(Pad);
            Controls.Add(DragArea);
            Controls.Add(Canvas);
        }

        public Control Find(string name)
        {
            Control[] found = Controls.Find(name, true);
            return found.Length > 0 ? found[0] : null;
        }
    }

    /// The test window, on its own STA thread with its own message loop, so
    /// the script can sleep and inject input while the window stays responsive.
    public static class Ui
    {
        static readonly object Gate = new object();
        static readonly List<string> events = new List<string>();
        static readonly ManualResetEvent ready = new ManualResetEvent(false);
        static string startError;
        public static SmokeForm Form;
        public static int Clicks;

        /// TedCat marks the events it replays with dwExtraInfo "TCAT".
        public const long TedCatMarker = 0x54434154;

        public static void Log(string who, string what, Point p)
        {
            Log(who, what, p, IntPtr.Zero);
        }

        /// One line: "tick control what x,y extra", extra being "TCAT" for
        /// TedCat's replays and "0x.." otherwise.
        public static void Log(string who, string what, Point p, IntPtr extraInfo)
        {
            long extra = extraInfo.ToInt64();
            string tag = extra == TedCatMarker ? "TCAT" : "0x" + extra.ToString("X");
            lock (Gate) events.Add(Environment.TickCount + " " + who + " " + what + " " + p.X + "," + p.Y + " " + tag);
        }

        public static string[] Events() { lock (Gate) return events.ToArray(); }
        public static void ClearEvents() { lock (Gate) events.Clear(); }

        public static string Start(int x, int y, int width, int height)
        {
            Rectangle bounds = new Rectangle(x, y, width, height);
            Thread thread = new Thread(delegate()
            {
                try
                {
                    Application.EnableVisualStyles();
                    Form = new SmokeForm(bounds);
                    Form.Shown += delegate { ready.Set(); };
                    Application.Run(Form);
                }
                catch (Exception e)
                {
                    startError = e.ToString();
                    ready.Set();
                }
            });
            thread.SetApartmentState(ApartmentState.STA);
            thread.IsBackground = true;
            thread.Start();
            if (!ready.WaitOne(20000)) return "the test window did not appear within 20 s";
            Thread.Sleep(300);
            return startError;
        }

        /// Screen rectangle (l, t, r, b) of a control's client area.
        public static int[] Rect(string name)
        {
            return (int[])Form.Invoke(new Func<int[]>(delegate
            {
                Control c = Form.Find(name);
                Rectangle r = c.RectangleToScreen(c.ClientRectangle);
                return new int[] { r.Left, r.Top, r.Right, r.Bottom };
            }));
        }

        public static void ResetKnob()
        {
            Form.Invoke(new Action(delegate { Form.Knob.Location = Form.KnobHome; }));
        }

        public static void Close()
        {
            try { Form.Invoke(new Action(delegate { Form.Close(); })); } catch (Exception) { }
        }
    }
}
'@
Add-Type -TypeDefinition $helpers -ReferencedAssemblies System.Windows.Forms, System.Drawing -Language CSharp

# ---------------------------------------------------------------------------
# Plumbing

$LogPath = Join-Path $env:LOCALAPPDATA 'TedCat\tedcat.log'
$SettingsPath = Join-Path $env:APPDATA 'TedCat\settings.json'
$VideosDir = Join-Path ([Environment]::GetFolderPath('MyVideos')) 'TedCat'

$VK_SHIFT = [uint16]0x10; $SC_SHIFT = [uint16]0x2A
$VK_ESCAPE = [uint16]0x1B; $SC_ESCAPE = [uint16]0x01

$script:Results = New-Object System.Collections.Generic.List[object]
$script:Notes = New-Object System.Collections.Generic.List[string]
$script:Ted = $null
$script:EnvironmentProblem = $null
$script:ShotCount = 0

function Note([string]$Text) {
    $script:Notes.Add($Text)
    Write-Host "    $Text"
}

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Skip([string]$Why) { throw "SKIP: $Why" }

function Save-Desktop([string]$Name) {
    $script:ShotCount++
    $path = Join-Path $ArtifactsDir ('{0:D2}-{1}.png' -f $script:ShotCount, $Name)
    $err = [TedCatSmoke.Win]::Shot($path)
    if ($err) { Write-Host "    (desktop screenshot failed: $err)" } else { Write-Host "    desktop screenshot: $path" }
}

function Invoke-Step([string]$Name, [scriptblock]$Body) {
    Write-Host ''
    Write-Host "=== $Name ==="
    $script:Notes = New-Object System.Collections.Generic.List[string]
    $result = 'passed'
    $why = $null
    if ($script:EnvironmentProblem -and $Name -ne 'after-exit') {
        $result = 'skipped'
        $why = $script:EnvironmentProblem
    } else {
        try {
            & $Body | Out-Host
        } catch {
            $message = $_.Exception.Message
            if ($message.StartsWith('SKIP: ')) {
                $result = 'skipped'
                $why = $message.Substring(6)
            } else {
                $result = 'failed'
                $why = '{0} (script line {1})' -f $message, $_.InvocationInfo.ScriptLineNumber
            }
        }
    }
    $evidence = @($script:Notes)
    if ($why) { $evidence = @($why) + $evidence }
    $script:Results.Add([pscustomobject]@{ Step = $Name; Result = $result; Evidence = ($evidence -join ' | ') })
    Write-Host ('>>> {0}: {1}' -f $Name, $result.ToUpper())
    if ($why) { Write-Host "    $why" }
    if ($result -eq 'failed') { Save-Desktop "failed-$Name" }
}

function Read-TedLog {
    if (Test-Path -LiteralPath $LogPath) { return [TedCatSmoke.Win]::ReadShared($LogPath) }
    return ''
}

function Get-LogMark { return (Read-TedLog).Length }

function Get-LogSince([int]$Mark) {
    $text = Read-TedLog
    if ($text.Length -le $Mark) { return '' }
    return $text.Substring($Mark)
}

# Waits for a regex in the log written after $Mark; returns the matching line or $null.
function Wait-Log([string]$Pattern, [int]$Mark = 0, [int]$TimeoutMs = 10000) {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        foreach ($line in ((Get-LogSince $Mark) -split "`r?`n")) {
            if ($line -match $Pattern) { return $line.Trim() }
        }
        Start-Sleep -Milliseconds 100
    } while ($watch.ElapsedMilliseconds -lt $TimeoutMs)
    return $null
}

function Wait-Until([scriptblock]$Condition, [int]$TimeoutMs = 5000) {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    do {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 50
    } while ($watch.ElapsedMilliseconds -lt $TimeoutMs)
    return [bool](& $Condition)
}

function Invoke-Input([bool]$Ok, [string]$What) {
    if (-not $Ok) { throw "SendInput failed during ${What}: $([TedCatSmoke.Input]::LastError)" }
}

function Center([int[]]$Rect) {
    return @([int](($Rect[0] + $Rect[2]) / 2), [int](($Rect[1] + $Rect[3]) / 2))
}

function Get-Events([string]$Control) {
    return @([TedCatSmoke.Ui]::Events() | Where-Object { ($_ -split ' ')[1] -eq $Control })
}

# "DOWN UP ..." for a control's button messages (moves left out).
function Get-ButtonSequence([string]$Control) {
    $names = @(Get-Events $Control | ForEach-Object { ($_ -split ' ')[2] } | Where-Object { $_ -in @('DOWN', 'UP', 'DBLCLK') })
    return ($names -join ' ')
}

# "DOWN:TCAT UP:0x0 ..." for a control's button messages: TCAT marks the
# events TedCat held and replayed, anything else came straight through.
function Get-TaggedSequence([string]$Control) {
    $tagged = @(Get-Events $Control | Where-Object { ($_ -split ' ')[2] -in @('DOWN', 'UP', 'DBLCLK') } | ForEach-Object { $f = $_ -split ' '; '{0}:{1}' -f $f[2], $f[4] })
    return ($tagged -join ' ')
}

function Get-VisibleOverlay {
    return @([TedCatSmoke.Win]::Windows('TedCat.Overlay.') | Where-Object { $_ -match 'visible=True' } | ForEach-Object { ($_ -split ' ')[0] })
}

# Press at (X1, Y1), hold still for HoldMs, drag to (X2, Y2), run WhileHeld,
# release. The button is always released, even when WhileHeld throws.
function Invoke-HoldDrag([int]$X1, [int]$Y1, [int]$X2, [int]$Y2, [int]$HoldMs = 600, [scriptblock]$WhileHeld = $null) {
    Invoke-Input ([TedCatSmoke.Input]::Move($X1, $Y1)) 'the move to the press point'
    Start-Sleep -Milliseconds 60
    Invoke-Input ([TedCatSmoke.Input]::Down($X1, $Y1)) 'the press'
    try {
        Start-Sleep -Milliseconds $HoldMs
        Invoke-Input ([TedCatSmoke.Input]::MovePath($X1, $Y1, $X2, $Y2, 20, 15)) 'the drag'
        Start-Sleep -Milliseconds 150
        if ($WhileHeld) { & $WhileHeld | Out-Host }
    } finally {
        if (-not [TedCatSmoke.Input]::Up($X2, $Y2)) { Write-Host "    (release failed: $([TedCatSmoke.Input]::LastError))" }
    }
}

function Assert-TedRunning {
    if ($script:Ted -and $script:Ted.HasExited) {
        throw ('TedCat exited unexpectedly with code 0x{0:X8}' -f $script:Ted.ExitCode)
    }
}

function Wait-Clicks([int]$Expected, [int]$TimeoutMs = 3000) {
    [void](Wait-Until { [TedCatSmoke.Ui]::Clicks -ge $Expected } $TimeoutMs)
    # Give a duplicate click time to show up.
    Start-Sleep -Milliseconds 700
    return [TedCatSmoke.Ui]::Clicks
}

# One quick click on the counter must count exactly once. With -Replayed the
# press and release must be TedCat's replays (held, then re-injected);
# without, they must have come straight through.
function Test-CounterClick([string]$What, [bool]$Replayed) {
    [TedCatSmoke.Ui]::ClearEvents()
    $before = [TedCatSmoke.Ui]::Clicks
    $c = Center ([TedCatSmoke.Ui]::Rect('Counter'))
    Invoke-Input ([TedCatSmoke.Input]::Click($c[0], $c[1], 40)) $What
    $after = Wait-Clicks ($before + 1)
    $tagged = Get-TaggedSequence 'Counter'
    Note "counter $before -> $after after one quick click at $($c[0]),$($c[1]); button got '$tagged'"
    Assert ($after -eq $before + 1) "a quick click on the counter changed it by $($after - $before) instead of exactly 1"
    if ($Replayed) {
        Assert ($tagged -eq 'DOWN:TCAT UP:TCAT') "expected TedCat's replayed press and release (DOWN:TCAT UP:TCAT), got '$tagged'"
    } else {
        Assert ($tagged -match '^DOWN:0x0 UP:0x0$') "expected the click to pass straight through (DOWN:0x0 UP:0x0), got '$tagged'"
    }
}

function Clipboard-HasImage {
    try { return [System.Windows.Forms.Clipboard]::ContainsImage() } catch { return $false }
}

# Known settings, written before each start (an existing file also means no first-run balloon).
function Write-Settings {
    New-Item -ItemType Directory -Force -Path (Split-Path $SettingsPath) | Out-Null
    [System.IO.File]::WriteAllText($SettingsPath, '{"enabled":true,"trigger":"HoldOnly","hold_millis":350,"play_sounds":false,"include_system_audio":true,"recordings_folder":null}')
}

function Start-TedCat {
    $process = Start-Process -FilePath $Exe -PassThru
    $null = $process.Handle  # keeps ExitCode readable after the process ends
    return $process
}

# Windows only drops its yellow capture border for desktop apps when the
# graphicsCaptureWithoutBorder consent allows it. Returns the values seen and
# whether a borderless capture is expected.
function Get-BorderlessConsent {
    $key = 'Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\graphicsCaptureWithoutBorder'
    $read = {
        param($path)
        $item = Get-ItemProperty -LiteralPath $path -Name Value -ErrorAction SilentlyContinue
        if ($item) { return [string]$item.Value }
        return 'missing'
    }
    $values = [ordered]@{
        'HKCU' = & $read "HKCU:\$key"
        'HKCU NonPackaged' = & $read "HKCU:\$key\NonPackaged"
        'HKLM' = & $read "HKLM:\$key"
    }
    $expected = $values['HKCU NonPackaged'] -eq 'Allow' -and $values['HKCU'] -ne 'Deny' -and $values['HKLM'] -ne 'Deny'
    $text = ($values.GetEnumerator() | ForEach-Object { '{0}={1}' -f $_.Key, $_.Value }) -join ', '
    return [pscustomobject]@{ Expected = $expected; Text = $text }
}

function Find-FFprobe {
    $command = Get-Command ffprobe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    foreach ($candidate in @("$env:ChocolateyInstall\bin\ffprobe.exe", 'C:\ProgramData\chocolatey\bin\ffprobe.exe')) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) { return $candidate }
    }
    return $null
}

function Invoke-Native([string]$File, [string[]]$Arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { return (& $File @Arguments 2>&1 | Out-String) } finally { $ErrorActionPreference = $previous }
}

# ---------------------------------------------------------------------------
# Steps

$exitCode = 0
try {
    Write-Host "TedCat smoke test: $Exe"
    Write-Host "Artifacts: $ArtifactsDir"

    Invoke-Step 'environment' {
        Note ("OS: {0} ({1})" -f (Get-CimInstance Win32_OperatingSystem).Caption, [Environment]::OSVersion.VersionString)
        Note ("PowerShell {0}, apartment {1}, 64-bit {2}" -f $PSVersionTable.PSVersion, [System.Threading.Thread]::CurrentThread.GetApartmentState(), [Environment]::Is64BitProcess)
        Note ("DPI awareness of this script: {0}; system DPI {1}" -f [TedCatSmoke.Win]::InitDpi(), [TedCatSmoke.Native]::GetDpiForSystem())
        Note ("session {0}, active console session {1}, input desktop: {2}" -f [System.Diagnostics.Process]::GetCurrentProcess().SessionId, [TedCatSmoke.Native]::WTSGetActiveConsoleSessionId(), [TedCatSmoke.Win]::InputDesktop())
        $integrity = @((whoami /groups) -match 'Mandatory Label' | ForEach-Object { ($_ -split '\s{2,}')[0] }) -join ','
        Note ("user: {0}; integrity: {1}" -f [Environment]::UserName, $integrity)
        $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
        $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        Note ("virtual screen {0}x{1} at {2},{3}; primary working area {4}" -f $vs.Width, $vs.Height, $vs.X, $vs.Y, $wa)
        foreach ($vc in @(Get-CimInstance Win32_VideoController)) { Note ("video: {0} ({1}x{2})" -f $vc.Name, $vc.CurrentHorizontalResolution, $vc.CurrentVerticalResolution) }
        $sound = @(Get-CimInstance Win32_SoundDevice -ErrorAction SilentlyContinue)
        Note ("sound devices: {0}" -f $(if ($sound.Count) { ($sound | ForEach-Object { $_.Name }) -join ', ' } else { 'none' }))
        try {
            $endpoints = @(Get-PnpDevice -Class AudioEndpoint -ErrorAction Stop | Where-Object { $_.Status -eq 'OK' })
            Note ("audio endpoints: {0}" -f $(if ($endpoints.Count) { ($endpoints | ForEach-Object { $_.FriendlyName }) -join ', ' } else { 'none' }))
        } catch { Note "audio endpoints: none ($($_.Exception.Message))" }
        Note ("borderless capture consent for desktop apps: {0}" -f (Get-BorderlessConsent).Text)
        foreach ($dll in @('mfplat.dll', 'mfreadwrite.dll', 'mfh264enc.dll', 'msmpeg2venc.dll', 'd3d11.dll', 'Windows.Graphics.dll')) {
            Note ("{0}: {1}" -f $dll, (Test-Path -LiteralPath (Join-Path $env:windir "System32\$dll")))
        }
        $ff = Find-FFprobe
        Note ("ffprobe: {0}" -f $(if ($ff) { $ff } else { 'not found' }))
        Note ("TedCat.exe: {0} bytes" -f (Get-Item -LiteralPath $Exe).Length)
        Note ("foreground window: {0}; cursor at {1}" -f [TedCatSmoke.Win]::Foreground(), [TedCatSmoke.Input]::Cursor())
    }

    Invoke-Step 'baseline' {
        # Clean slate: no TedCat, no log, settings written so there is no first-run balloon.
        Get-Process -Name TedCat -ErrorAction SilentlyContinue | Stop-Process -Force
        Remove-Item -LiteralPath $LogPath -Force -ErrorAction SilentlyContinue
        Write-Settings
        Note "settings: $(Get-Content -Raw -LiteralPath $SettingsPath)"

        $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        $startError = [TedCatSmoke.Ui]::Start($wa.X + 20, $wa.Y + 20, [Math]::Min(1000, $wa.Width - 40), [Math]::Min(700, $wa.Height - 40))
        if ($startError) { throw "test window failed: $startError" }
        $c = Center ([TedCatSmoke.Ui]::Rect('Counter'))
        Note ("test window up; counter at {0},{1}; window there: {2}" -f $c[0], $c[1], [TedCatSmoke.Win]::WindowAt($c[0], $c[1]))

        if (-not [TedCatSmoke.Input]::Click($c[0], $c[1], 40)) {
            $script:EnvironmentProblem = "no interactive input on this desktop: $([TedCatSmoke.Input]::LastError); input desktop $([TedCatSmoke.Win]::InputDesktop())"
            Skip $script:EnvironmentProblem
        }
        Note "cursor after the click: $([TedCatSmoke.Input]::Cursor())"
        $after = Wait-Clicks 1
        Note "button got '$(Get-TaggedSequence 'Counter')'"
        if ($after -ne 1) {
            $script:EnvironmentProblem = "SendInput succeeded but the click never reached the test window (count $after, window under the cursor: $([TedCatSmoke.Win]::WindowAt($c[0], $c[1])))"
            Save-Desktop 'baseline-no-click'
            Skip $script:EnvironmentProblem
        }
        Note 'a SendInput click reaches the test window without TedCat (count 0 -> 1)'
    }

    Invoke-Step 'start' {
        Assert (Test-Path -LiteralPath $Exe) "TedCat.exe not found at $Exe"
        $script:Ted = Start-TedCat
        Note "started TedCat.exe, pid $($script:Ted.Id)"
        $line = Wait-Log 'input hook status: (Active|Unavailable.*)' 0 15000
        if (-not $line) {
            if ($script:Ted.HasExited) { throw ('TedCat exited during start-up with code 0x{0:X8}; log: {1}' -f $script:Ted.ExitCode, (Read-TedLog)) }
            throw 'TedCat never reported its input hook status within 15 s'
        }
        Note $line
        $installed = Wait-Log '\[input\] Input hooks installed' 0 3000
        if ($installed) { Note $installed }
        if ($line -match 'Unavailable') {
            $script:EnvironmentProblem = "Windows refused TedCat's low-level hooks on this runner: $line"
            Skip $script:EnvironmentProblem
        }
        Assert-TedRunning
        Note ("TedCat windows: {0}" -f ([TedCatSmoke.Win]::Windows('TedCat.') -join '; '))
        $main = [TedCatSmoke.Win]::Find('TedCat.Main')
        Assert ($main -ne [IntPtr]::Zero) 'no TedCat.Main window'
        [void](Wait-Until { [TedCatSmoke.Win]::TrayIcon($main, 1).StartsWith('rect=') } 5000)
        $tray = [TedCatSmoke.Win]::TrayIcon($main, 1)
        Note "tray icon: $tray"
        Assert ($tray.StartsWith('rect=')) "the shell does not know TedCat's tray icon ($tray)"

        # Single instance: a second TedCat must leave at once, so two hooks never hold the same click.
        $mark = Get-LogMark
        $second = Start-TedCat
        $exited = $second.WaitForExit(5000)
        if (-not $exited) { Stop-Process -Id $second.Id -Force -ErrorAction SilentlyContinue }
        $line = Wait-Log 'another TedCat is already running; exiting' $mark 2000
        Note ("second TedCat (pid {0}) exited: {1}, code {2}; log: {3}" -f $second.Id, $exited, $(if ($exited) { $second.ExitCode } else { '-' }), $(if ($line) { $line } else { '(none)' }))
        Assert ($exited -and $second.ExitCode -eq 0) 'a second TedCat kept running (or failed) instead of leaving quietly'
        Assert ($null -ne $line) 'the second TedCat did not log that another one is running'
        Assert-TedRunning
    }

    Invoke-Step 'replay-click' {
        Test-CounterClick 'the quick click' $true
        Assert-TedRunning
    }

    Invoke-Step 'replay-double-click' {
        [TedCatSmoke.Ui]::ClearEvents()
        $c = Center ([TedCatSmoke.Ui]::Rect('Pad'))
        Invoke-Input ([TedCatSmoke.Input]::DoubleClick($c[0], $c[1], 30, 70)) 'the double click'
        [void](Wait-Until { (Get-ButtonSequence 'Pad') -eq 'DOWN UP DBLCLK UP' } 3000)
        Start-Sleep -Milliseconds 700
        $sequence = Get-ButtonSequence 'Pad'
        $doubles = @(Get-Events 'Pad' | Where-Object { $_ -match 'MouseDoubleClick' }).Count
        Note "pad received: $sequence; MouseDoubleClick events: $doubles"
        Assert ($sequence -eq 'DOWN UP DBLCLK UP') "expected 'DOWN UP DBLCLK UP' on the pad, got '$sequence'"
        Assert ($doubles -eq 1) "expected exactly one MouseDoubleClick, got $doubles"
        $tagged = Get-TaggedSequence 'Pad'
        Note "with replay markers: $tagged"
        Assert ($tagged -eq 'DOWN:TCAT UP:TCAT DBLCLK:TCAT UP:TCAT') "expected all four to be TedCat's replays, got '$tagged'"
        Assert-TedRunning
    }

    Invoke-Step 'replay-drag' {
        [TedCatSmoke.Ui]::ResetKnob()
        Start-Sleep -Milliseconds 200
        [TedCatSmoke.Ui]::ClearEvents()
        $before = [TedCatSmoke.Ui]::Rect('Knob')
        $a = Center $before
        $dx = 200
        # Moves right away (10 px steps every 10 ms): a normal drag, not a hold.
        Invoke-Input ([TedCatSmoke.Input]::Drag($a[0], $a[1], $a[0] + $dx, $a[1], 0, 10, 10)) 'the quick drag'
        Start-Sleep -Milliseconds 800
        $after = [TedCatSmoke.Ui]::Rect('Knob')
        $moved = @(($after[0] - $before[0]), ($after[1] - $before[1]))
        $events = @(Get-Events 'Knob' | ForEach-Object { ($_ -split ' ')[2] })
        $sequence = (($events | Select-Object -Unique) -join ' ')
        Note ("knob moved by {0},{1} (expected {2},0); knob saw {3} events: first {4}, last {5}, {6} moves" -f $moved[0], $moved[1], $dx, $events.Count, $(if ($events.Count) { $events[0] } else { '-' }), $(if ($events.Count) { $events[-1] } else { '-' }), @($events | Where-Object { $_ -eq 'MOVE' }).Count)
        $first = @(Get-Events 'Knob') | Select-Object -First 1
        if ($first) { Note "first knob event: $first (press was at $($a[0]),$($a[1]))" }
        Assert ($events.Count -ge 3 -and $events[0] -eq 'DOWN' -and $events[-1] -eq 'UP') "expected DOWN, MOVE..., UP on the knob, got '$($events -join ' ')'"
        Assert (@($events | Where-Object { $_ -eq 'DOWN' }).Count -eq 1 -and @($events | Where-Object { $_ -eq 'UP' }).Count -eq 1) "expected exactly one DOWN and one UP, got '$sequence'"
        Assert (@($events | Where-Object { $_ -eq 'MOVE' }).Count -ge 1) 'no MOVE between DOWN and UP'
        Assert ([Math]::Abs($moved[0] - $dx) -le 2 -and [Math]::Abs($moved[1]) -le 2) "the knob moved by $($moved[0]),$($moved[1]) instead of $dx,0"
        $tagged = Get-TaggedSequence 'Knob'
        Note "with replay markers: $tagged (the held press is replayed, the rest of the drag flows through)"
        Assert ($tagged -match '^DOWN:TCAT UP:') "expected the press to be TedCat's replay, got '$tagged'"
        [TedCatSmoke.Ui]::ResetKnob()
        Assert-TedRunning
    }

    Invoke-Step 'screenshot' {
        Assert ([TedCatSmoke.Clip]::Clear()) 'could not clear the clipboard'
        Assert (-not (Clipboard-HasImage)) 'the clipboard still holds an image after clearing it'
        [TedCatSmoke.Ui]::ClearEvents()
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $w = 320; $h = 200
        $x1 = $canvas[0] + 40; $y1 = $canvas[1] + 40
        $x2 = $x1 + $w; $y2 = $y1 + $h
        $mark = Get-LogMark
        $script:overlay = @()
        Invoke-HoldDrag $x1 $y1 $x2 $y2 600 { $script:overlay = @(Get-VisibleOverlay) }
        $hasImage = Wait-Until { Clipboard-HasImage } 5000
        Note "overlay while selecting: $($script:overlay -join ', ')"
        Assert ('TedCat.Overlay.Dim' -in $script:overlay -and 'TedCat.Overlay.Frame' -in $script:overlay -and 'TedCat.Overlay.Label' -in $script:overlay) 'the selection overlay (dim, frame, label) was not on screen while selecting' 
        $line = Wait-Log 'screenshot \d+x\d+ copied|screenshot failed' $mark 2000
        Note "log: $(if ($line) { $line } else { '(no screenshot line)' })"
        Note ("clipboard formats: {0}; owner pid {1} (TedCat {2})" -f ([TedCatSmoke.Clip]::Formats() -join ', '), [TedCatSmoke.Clip]::OwnerPid(), $script:Ted.Id)
        Assert $hasImage 'no image on the clipboard within 5 s of the release'
        $image = [System.Windows.Forms.Clipboard]::GetImage()
        Assert ($null -ne $image) 'Clipboard.GetImage() returned nothing'
        Note ("Clipboard.GetImage(): {0}x{1} (expected {2}x{3})" -f $image.Width, $image.Height, $w, $h)
        Assert ([Math]::Abs($image.Width - $w) -le 2 -and [Math]::Abs($image.Height - $h) -le 2) "image is $($image.Width)x$($image.Height), expected about ${w}x${h}"
        $png = [TedCatSmoke.Clip]::PngSize()
        Note "PNG format: $(if ($png) { $png } else { 'missing' })"
        Assert ($null -ne $png) 'the registered "PNG" clipboard format is missing'
        Assert ($png -eq "$($image.Width)x$($image.Height)") "PNG is $png but the bitmap is $($image.Width)x$($image.Height)"
        # The selection's middle is plain canvas: the overlay's dimming must not be in the picture.
        $pixel = $image.GetPixel([int]($image.Width / 2), [int]($image.Height * 3 / 4))
        $expected = [TedCatSmoke.SmokeForm]::CanvasColor
        Note ("pixel in the image: {0},{1},{2} (canvas is {3},{4},{5})" -f $pixel.R, $pixel.G, $pixel.B, $expected.R, $expected.G, $expected.B)
        $image.Save((Join-Path $ArtifactsDir 'screenshot-from-clipboard.png'), [System.Drawing.Imaging.ImageFormat]::Png)
        $image.Dispose()
        Assert ([Math]::Abs($pixel.R - $expected.R) -le 8 -and [Math]::Abs($pixel.G - $expected.G) -le 8 -and [Math]::Abs($pixel.B - $expected.B) -le 8) 'the screenshot does not show the canvas colour (overlay captured, or a black/blank capture)'
        $leaked = @(Get-Events 'Canvas' | Where-Object { $_ -match ' (DOWN|UP|DBLCLK) ' })
        Note "button messages that reached the window under the selection: $($leaked.Count)"
        Assert ($leaked.Count -eq 0) "the held press leaked to the window underneath: $($leaked -join '; ')"
        Start-Sleep -Milliseconds 500
        $left = @(Get-VisibleOverlay)
        Note "overlay windows still visible after the capture: $(if ($left.Count) { $left -join ', ' } else { 'none' })"
        Assert ($left.Count -eq 0) "the overlay stayed on screen after the screenshot: $($left -join ', ')"
        Assert-TedRunning
    }

    Invoke-Step 'escape-cancel' {
        Assert ([TedCatSmoke.Clip]::Clear()) 'could not clear the clipboard'
        [TedCatSmoke.Ui]::ClearEvents()
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $x1 = $canvas[0] + 40; $y1 = $canvas[1] + 40
        $mark = Get-LogMark
        $script:overlay = @(); $script:overlayAfterEscape = @()
        Invoke-HoldDrag $x1 $y1 ($x1 + 300) ($y1 + 180) 600 {
            $script:overlay = @(Get-VisibleOverlay)
            Invoke-Input ([TedCatSmoke.Input]::Tap($VK_ESCAPE, $SC_ESCAPE, 30)) 'Escape'
            Start-Sleep -Milliseconds 400
            $script:overlayAfterEscape = @(Get-VisibleOverlay)
        }
        Start-Sleep -Milliseconds 1500
        $shot = Wait-Log 'screenshot \d+x\d+ copied' $mark 100
        $hasImage = Clipboard-HasImage
        $tagged = Get-TaggedSequence 'Canvas'
        Note "overlay before Escape: $($script:overlay -join ', '); after Escape: $(if ($script:overlayAfterEscape.Count) { $script:overlayAfterEscape -join ', ' } else { 'none' })"
        Note "clipboard image: $hasImage; canvas got '$tagged' (the release after a cancel is passed on by design)"
        Assert ('TedCat.Overlay.Dim' -in $script:overlay) 'the overlay never appeared, so there was nothing to cancel'
        Assert ($script:overlayAfterEscape.Count -eq 0) "Escape did not close the overlay: $($script:overlayAfterEscape -join ', ')"
        Assert (-not $hasImage -and -not $shot) 'a screenshot was taken although Escape cancelled the selection'
        Assert ($tagged -notmatch 'DOWN') "the cancelled press reached the window underneath: '$tagged'"
        Assert-TedRunning
    }

    Invoke-Step 'recording' {
        $existing = @()
        if (Test-Path -LiteralPath $VideosDir) { $existing = @(Get-ChildItem -LiteralPath $VideosDir -Filter *.mp4 | ForEach-Object { $_.FullName }) }
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $w = [Math]::Min(480, $canvas[2] - $canvas[0] - 60); $w -= $w % 2
        $h = [Math]::Min(320, $canvas[3] - $canvas[1] - 60); $h -= $h % 2
        Assert ($w -ge 400 -and $h -ge 300) "the screen is too small for a 400x300 recording region (canvas $($canvas -join ','))"
        $x1 = $canvas[0] + 30; $y1 = $canvas[1] + 30
        $x2 = $x1 + $w; $y2 = $y1 + $h
        $yellowBefore = 0
        $edgesBefore = [TedCatSmoke.Win]::CaptureBorder([ref]$yellowBefore)
        $consent = Get-BorderlessConsent
        $mark = Get-LogMark
        $script:overlay = @()
        Invoke-Input ([TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $false)) 'Shift down'
        try {
            Start-Sleep -Milliseconds 80
            Invoke-HoldDrag $x1 $y1 $x2 $y2 600 { $script:overlay = @(Get-VisibleOverlay) }
            Start-Sleep -Milliseconds 80
        } finally {
            [void][TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $true)
        }
        Note "Shift + hold + drag from $x1,$y1 to $x2,$y2 (${w}x${h}); overlay while selecting: $($script:overlay -join ', ')"
        # Checked once the recording is stopped, so a failure never leaves it running.
        $problems = New-Object System.Collections.Generic.List[string]
        if (-not ('TedCat.Overlay.Dim' -in $script:overlay -and 'TedCat.Overlay.Frame' -in $script:overlay)) {
            $problems.Add('the recording selection overlay was not on screen while selecting')
        }

        $script:hud = $null
        [void](Wait-Until { $script:hud = [TedCatSmoke.Win]::FindVisible('TedCat.Hud'); ($null -ne $script:hud) -or [bool]((Get-LogSince $mark) -match 'recording could not start|too small to record') } 20000)
        $startLine = Wait-Log 'starting region recording.*' $mark 500
        if ($startLine) { Note $startLine }
        foreach ($l in @((Get-LogSince $mark) -split "`r?`n" | Where-Object { $_ -match 'sink writer|D3D11|WARP|capture|without system audio|audio' })) { Note ("log: " + $l.Trim()) }
        $failure = Wait-Log 'recording could not start: .*|too small to record' $mark 200
        if ($failure) { throw "TedCat did not start recording: $failure" }
        Assert ($null -ne $script:hud) 'no recording pill (TedCat.Hud) appeared within 20 s'
        Note ("recording pill at {0}" -f ($script:hud -join ','))
        Note ("TedCat windows: {0}" -f ([TedCatSmoke.Win]::Windows('TedCat.') -join '; '))
        Start-Sleep -Milliseconds 700
        Save-Desktop 'recording-in-progress'
        $yellowDuring = 0
        $edgesDuring = [TedCatSmoke.Win]::CaptureBorder([ref]$yellowDuring)
        Note "screen edges before recording: $edgesBefore; while recording: $edgesDuring ($yellowDuring of 4 yellow)"
        $borderLine = Wait-Log 'capture border cannot be disabled.*' $mark 100
        if ($borderLine) {
            Note "TedCat could not turn off the capture border here: $borderLine"
        } elseif (-not $consent.Expected) {
            Note "border check not applicable: Windows does not let desktop apps drop the capture border here (consent store: $($consent.Text))"
        } elseif ($yellowDuring -ge 3 -and $yellowBefore -lt 3) {
            $problems.Add("Windows draws its yellow capture border around the whole screen while recording ($yellowDuring of 4 edges yellow) although borderless capture is allowed ($($consent.Text))")
        } else {
            Note "no capture border while recording (borderless capture allowed: $($consent.Text))"
        }

        Start-Sleep -Seconds $RecordSeconds
        $pill = [TedCatSmoke.Win]::FindVisible('TedCat.Hud')
        Assert ($null -ne $pill) 'the recording pill disappeared before it was clicked'
        $p = Center $pill
        Note ("window at the pill centre: {0}" -f [TedCatSmoke.Win]::WindowAt($p[0], $p[1]))
        Invoke-Input ([TedCatSmoke.Input]::Click($p[0], $p[1], 40)) 'the click on the pill'
        $stopLine = Wait-Log 'stopping the recording' $mark 5000
        Assert ($null -ne $stopLine) 'clicking the pill did not stop the recording (no "stopping the recording" in the log)'
        $saved = Wait-Log 'saved .* bytes|recording failed: .*' $mark 30000
        Assert ($null -ne $saved) 'the recording was not finalized within 30 s'
        Note $saved
        Assert ($saved -notmatch 'recording failed') "TedCat could not save the recording: $saved"
        $noAudio = Wait-Log 'recording without system audio.*' $mark 100
        if ($noAudio) { Note "no audio device, as expected on a runner: $noAudio" } else { Note 'TedCat recorded with system audio' }

        $new = @(Get-ChildItem -LiteralPath $VideosDir -Filter *.mp4 | Where-Object { $existing -notcontains $_.FullName })
        Assert ($new.Count -eq 1) "expected one new .mp4 in $VideosDir, found $($new.Count)"
        $file = $new[0]
        Note ("{0}: {1} bytes" -f $file.Name, $file.Length)
        Assert ($file.Length -gt 0) 'the recording is empty'
        Copy-Item -LiteralPath $file.FullName -Destination $ArtifactsDir

        $ffprobe = Find-FFprobe
        if (-not $ffprobe) {
            Note 'ffprobe not available: stream checks SKIPPED'
        } else {
            $json = Invoke-Native $ffprobe @('-v', 'error', '-show_entries', 'stream=codec_type,codec_name,width,height,duration:format=duration,format_name', '-of', 'json', $file.FullName)
            $info = $json | ConvertFrom-Json
            $video = @($info.streams | Where-Object { $_.codec_type -eq 'video' })
            $audio = @($info.streams | Where-Object { $_.codec_type -eq 'audio' })
            Assert ($video.Count -eq 1) "expected one video stream, ffprobe says: $json"
            $duration = [double]$info.format.duration
            Note ("ffprobe: {0}, video {1} {2}x{3}, {4} audio stream(s){5}, duration {6:N2} s" -f $info.format.format_name, $video[0].codec_name, $video[0].width, $video[0].height, $audio.Count, $(if ($audio.Count) { " ($($audio[0].codec_name))" } else { '' }), $duration)
            Assert ($video[0].codec_name -eq 'h264') "video codec is $($video[0].codec_name), expected h264"
            Assert ([int]$video[0].width -eq $w -and [int]$video[0].height -eq $h) "video is $($video[0].width)x$($video[0].height), expected ${w}x${h}"
            Assert (([int]$video[0].width % 2) -eq 0 -and ([int]$video[0].height % 2) -eq 0) 'odd video dimensions'
            Assert ($duration -ge ($RecordSeconds - 0.5) -and $duration -le ($RecordSeconds + 6)) "duration $duration s is far from the ~$RecordSeconds s recorded"
            if ($noAudio) { Assert ($audio.Count -eq 0) 'TedCat said there is no audio, but the file has an audio stream' }
        }
        # The saved card replaces the pill.
        $card = Wait-Until { $null -ne [TedCatSmoke.Win]::FindVisible('TedCat.Card') } 3000
        Note ("saved card shown: {0}" -f $card)
        Assert ($problems.Count -eq 0) ($problems -join '; ')
        Assert-TedRunning
    }

    Invoke-Step 'kill-switch' {
        $mark = Get-LogMark
        for ($i = 0; $i -lt 3; $i++) {
            Invoke-Input ([TedCatSmoke.Input]::Tap($VK_ESCAPE, $SC_ESCAPE, 30)) 'Escape'
            Start-Sleep -Milliseconds 150
        }
        $switch = Wait-Log 'Emergency switch.*' $mark 3000
        $status = Wait-Log 'input hook status: KillSwitch' $mark 3000
        Note "log: $(if ($switch) { $switch } else { '(no emergency switch line)' })"
        Note "log: $(if ($status) { $status } else { '(no KillSwitch status line)' })"
        Assert ($null -ne $switch -and $null -ne $status) 'Escape x3 did not trip the emergency switch'

        Assert ([TedCatSmoke.Clip]::Clear()) 'could not clear the clipboard'
        [TedCatSmoke.Ui]::ClearEvents()
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $x1 = $canvas[0] + 40; $y1 = $canvas[1] + 40
        $mark = Get-LogMark
        $script:overlay = @()
        Invoke-HoldDrag $x1 $y1 ($x1 + 320) ($y1 + 200) 600 { $script:overlay = @(Get-VisibleOverlay) }
        Start-Sleep -Seconds 3
        $hasImage = Clipboard-HasImage
        $shot = Wait-Log 'screenshot \d+x\d+ copied' $mark 100
        $tagged = Get-TaggedSequence 'Canvas'
        Note ("after hold + drag: overlay [{0}], clipboard image {1}, formats [{2}], canvas got '{3}'" -f ($script:overlay -join ', '), $hasImage, ([TedCatSmoke.Clip]::Formats() -join ', '), $tagged)
        Assert ($script:overlay.Count -eq 0) "the overlay appeared although the gesture is off: $($script:overlay -join ', ')"
        Assert (-not $hasImage -and -not $shot) 'a screenshot was taken although the gesture is off'
        Assert ($tagged -eq 'DOWN:0x0 UP:0x0') "with the gesture off the press must pass straight through, not held or replayed; the canvas got '$tagged'"
        Test-CounterClick 'the click after the kill switch' $false
        Assert-TedRunning
    }

    Invoke-Step 'after-exit' {
        if (-not $script:Ted) { Skip 'TedCat was never started' }
        if (-not $script:Ted.HasExited) {
            Stop-Process -Id $script:Ted.Id -Force
            $script:Ted.WaitForExit(10000) | Out-Null
        }
        Note ('TedCat stopped (exit code 0x{0:X8})' -f $script:Ted.ExitCode)
        if ($script:EnvironmentProblem) { Skip $script:EnvironmentProblem }
        Test-CounterClick 'the click after TedCat exited' $false
    }

    # The top requirement: a TedCat that dies mid-gesture must leave clicks working.
    Invoke-Step 'crash-mid-gesture' {
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $x = $canvas[0] + 60; $y = $canvas[1] + 140
        foreach ($phase in @('pending', 'selecting')) {
            Write-Settings
            $mark = Get-LogMark
            $script:Ted = Start-TedCat
            Assert ($null -ne (Wait-Log 'input hook status: Active' $mark 15000)) "TedCat did not start again (before the '$phase' crash)"
            [TedCatSmoke.Ui]::ClearEvents()
            Invoke-Input ([TedCatSmoke.Input]::Move($x, $y)) 'the move'
            Start-Sleep -Milliseconds 60
            Invoke-Input ([TedCatSmoke.Input]::Down($x, $y)) 'the press'
            $overlay = @()
            try {
                if ($phase -eq 'selecting') {
                    Start-Sleep -Milliseconds 600
                    Invoke-Input ([TedCatSmoke.Input]::MovePath($x, $y, $x + 200, $y + 120, 10, 15)) 'the drag'
                    Start-Sleep -Milliseconds 150
                    $overlay = @(Get-VisibleOverlay)
                } else {
                    Start-Sleep -Milliseconds 100  # well inside the 350 ms hold: the press is held, undecided
                }
                Stop-Process -Id $script:Ted.Id -Force
                [void]$script:Ted.WaitForExit(10000)
                Start-Sleep -Milliseconds 300
            } finally {
                [void][TedCatSmoke.Input]::Up($x, $y)
            }
            Start-Sleep -Milliseconds 300
            $left = @(Get-VisibleOverlay)
            Note ("killed TedCat while the press was {0}: overlay before {1}, after {2}; canvas got '{3}'" -f $phase, $(if ($overlay.Count) { $overlay -join '+' } else { 'none' }), $(if ($left.Count) { $left -join '+' } else { 'none' }), (Get-TaggedSequence 'Canvas'))
            if ($phase -eq 'selecting') { Assert ('TedCat.Overlay.Dim' -in $overlay) 'the selection never started, so the crash was not mid-selection' }
            Assert ($left.Count -eq 0) "overlay windows survived TedCat: $($left -join ', ')"
            Test-CounterClick "the click after TedCat died while the press was $phase" $false
        }
    }

    # =======================================================================
    # Recording fixes: delay-loaded Media Foundation, regions scaled down to
    # what the encoder accepts, and display changes during a recording.
    #
    #   recording-delay-load
    #                 mfplat.dll and mfreadwrite.dll are delay-load imports of
    #                 TedCat.exe, not normal ones, so it starts on Windows N
    #                 without the Media Feature Pack (dumpbin, or llvm-readobj).
    #   recording-downscale
    #                 with TEDCAT_TEST_MAX_VIDEO_SIZE=240x240 a 480x320 region
    #                 takes the path a region wider than 4096 px takes for
    #                 real: cropped at 480x320, scaled to 240x160 (GPU or CPU,
    #                 logged), the file is 240x160 and shows the region.
    #   recording-display-change
    #                 the display resolution changes mid-recording: TedCat
    #                 interrupts the recording with a clear reason and still
    #                 saves what it captured. The resolution and the test
    #                 window are restored afterwards.
    #
    # Each step starts its own TedCat and stops it at the end.

    $recordFixHelpers = @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Windows.Forms;

namespace TedCatSmokeRecordFix
{
    /// A display mode of the primary display.
    public class Mode
    {
        public Display.DEVMODE Dev;
        public int Width { get { return (int)Dev.dmPelsWidth; } }
        public int Height { get { return (int)Dev.dmPelsHeight; } }
        public override string ToString()
        {
            return Dev.dmPelsWidth + "x" + Dev.dmPelsHeight + " " + Dev.dmBitsPerPel + " bpp " + Dev.dmDisplayFrequency + " Hz";
        }
    }

    public static class Display
    {
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct DEVMODE
        {
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
            public ushort dmSpecVersion;
            public ushort dmDriverVersion;
            public ushort dmSize;
            public ushort dmDriverExtra;
            public uint dmFields;
            public int dmPositionX;
            public int dmPositionY;
            public uint dmDisplayOrientation;
            public uint dmDisplayFixedOutput;
            public short dmColor;
            public short dmDuplex;
            public short dmYResolution;
            public short dmTTOption;
            public short dmCollate;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
            public ushort dmLogPixels;
            public uint dmBitsPerPel;
            public uint dmPelsWidth;
            public uint dmPelsHeight;
            public uint dmDisplayFlags;
            public uint dmDisplayFrequency;
            public uint dmICMMethod;
            public uint dmICMIntent;
            public uint dmMediaType;
            public uint dmDitherType;
            public uint dmReserved1;
            public uint dmReserved2;
            public uint dmPanningWidth;
            public uint dmPanningHeight;
        }

        const int ENUM_CURRENT_SETTINGS = -1;
        const uint CDS_TEST = 0x2;

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        static extern bool EnumDisplaySettings(string device, int mode, ref DEVMODE devMode);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        static extern int ChangeDisplaySettingsEx(string device, ref DEVMODE devMode, IntPtr hwnd, uint flags, IntPtr param);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "ChangeDisplaySettingsExW")]
        static extern int ChangeDisplaySettingsExNull(string device, IntPtr devMode, IntPtr hwnd, uint flags, IntPtr param);
        [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);

        static DEVMODE Blank()
        {
            DEVMODE dm = new DEVMODE();
            dm.dmSize = (ushort)Marshal.SizeOf(typeof(DEVMODE));
            return dm;
        }

        public static Mode Current()
        {
            DEVMODE dm = Blank();
            if (!EnumDisplaySettings(null, ENUM_CURRENT_SETTINGS, ref dm)) return null;
            Mode mode = new Mode();
            mode.Dev = dm;
            return mode;
        }

        /// Every mode of the primary display, as "WxH bpp Hz".
        public static string[] All()
        {
            List<string> found = new List<string>();
            DEVMODE dm = Blank();
            for (int i = 0; EnumDisplaySettings(null, i, ref dm); i++)
            {
                Mode mode = new Mode();
                mode.Dev = dm;
                string text = mode.ToString();
                if (!found.Contains(text)) found.Add(text);
                dm = Blank();
            }
            return found.ToArray();
        }

        /// A mode with another size and the same colour depth: 800x600 if
        /// offered, else the largest one smaller than `current`, else any.
        public static Mode FindOther(Mode current)
        {
            Mode preferred = null, smaller = null, any = null;
            DEVMODE dm = Blank();
            for (int i = 0; EnumDisplaySettings(null, i, ref dm); i++)
            {
                if (dm.dmBitsPerPel == current.Dev.dmBitsPerPel
                    && (dm.dmPelsWidth != current.Dev.dmPelsWidth || dm.dmPelsHeight != current.Dev.dmPelsHeight))
                {
                    Mode mode = new Mode();
                    mode.Dev = dm;
                    if (dm.dmPelsWidth == 800 && dm.dmPelsHeight == 600) preferred = mode;
                    if (dm.dmPelsWidth <= current.Dev.dmPelsWidth && dm.dmPelsHeight <= current.Dev.dmPelsHeight
                        && (smaller == null || (long)dm.dmPelsWidth * dm.dmPelsHeight > (long)smaller.Width * smaller.Height))
                    {
                        smaller = mode;
                    }
                    if (any == null) any = mode;
                }
                dm = Blank();
            }
            return preferred ?? smaller ?? any;
        }

        /// ChangeDisplaySettingsEx for the primary display, for this session
        /// only (never written to the registry). 0 means DISP_CHANGE_SUCCESSFUL.
        public static int Apply(Mode mode, bool testOnly)
        {
            DEVMODE dm = mode.Dev;
            dm.dmSize = (ushort)Marshal.SizeOf(typeof(DEVMODE));
            return ChangeDisplaySettingsEx(null, ref dm, IntPtr.Zero, testOnly ? CDS_TEST : 0u, IntPtr.Zero);
        }

        /// Back to the mode stored in the registry.
        public static int Reset()
        {
            return ChangeDisplaySettingsExNull(null, IntPtr.Zero, IntPtr.Zero, 0, IntPtr.Zero);
        }

        /// The primary screen's size right now ("WxH"), from GetSystemMetrics.
        public static string ScreenSize()
        {
            return GetSystemMetrics(0) + "x" + GetSystemMetrics(1);
        }

        public static int[] FormBounds(Form form)
        {
            return (int[])form.Invoke(new Func<int[]>(delegate
            {
                Rectangle b = form.Bounds;
                return new int[] { b.X, b.Y, b.Width, b.Height };
            }));
        }

        public static void SetFormBounds(Form form, int[] bounds)
        {
            form.Invoke(new Action(delegate { form.Bounds = new Rectangle(bounds[0], bounds[1], bounds[2], bounds[3]); }));
        }
    }
}
'@
    Add-Type -TypeDefinition $recordFixHelpers -ReferencedAssemblies System.Windows.Forms, System.Drawing -Language CSharp

    # dumpbin from the newest Visual Studio, else dumpbin or llvm-readobj on the PATH or in the LLVM folder.
    function Find-RecordFixImportTool {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (Test-Path -LiteralPath $vswhere) {
            $found = @((Invoke-Native $vswhere @('-latest', '-products', '*', '-find', 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe')) -split "`r?`n" | Where-Object { $_ -match 'dumpbin\.exe$' -and (Test-Path -LiteralPath $_.Trim()) })
            if ($found.Count) { return [pscustomobject]@{ Kind = 'dumpbin'; Path = $found[0].Trim() } }
        }
        $command = Get-Command dumpbin -ErrorAction SilentlyContinue
        if ($command) { return [pscustomobject]@{ Kind = 'dumpbin'; Path = $command.Source } }
        $command = Get-Command llvm-readobj -ErrorAction SilentlyContinue
        if ($command) { return [pscustomobject]@{ Kind = 'llvm-readobj'; Path = $command.Source } }
        $candidate = Join-Path $env:ProgramFiles 'LLVM\bin\llvm-readobj.exe'
        if (Test-Path -LiteralPath $candidate) { return [pscustomobject]@{ Kind = 'llvm-readobj'; Path = $candidate } }
        return $null
    }

    function Stop-RecordFixTedCat {
        if ($script:Ted -and -not $script:Ted.HasExited) {
            Stop-Process -Id $script:Ted.Id -Force -ErrorAction SilentlyContinue
            [void]$script:Ted.WaitForExit(10000)
        }
        Get-Process -Name TedCat -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }

    # A fresh TedCat with known settings; returns the log mark taken before it started.
    function Start-RecordFixTedCat {
        Stop-RecordFixTedCat
        Write-Settings
        $mark = Get-LogMark
        $script:Ted = Start-TedCat
        Assert ($null -ne (Wait-Log 'input hook status: Active' $mark 15000)) 'TedCat did not start (no "input hook status: Active")'
        Note "TedCat started, pid $($script:Ted.Id)"
        return $mark
    }

    function Get-RecordFixVideos {
        if (-not (Test-Path -LiteralPath $VideosDir)) { return @() }
        return @(Get-ChildItem -LiteralPath $VideosDir -Filter *.mp4 | ForEach-Object { $_.FullName })
    }

    # Shift + hold + drag over the region, then waits for the recording pill.
    function Start-RecordFixRecording([int]$X1, [int]$Y1, [int]$W, [int]$H, [int]$Mark) {
        Invoke-Input ([TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $false)) 'Shift down'
        try {
            Start-Sleep -Milliseconds 80
            Invoke-HoldDrag $X1 $Y1 ($X1 + $W) ($Y1 + $H) 600
            Start-Sleep -Milliseconds 80
        } finally {
            [void][TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $true)
        }
        $script:recordFixHud = $null
        [void](Wait-Until { $script:recordFixHud = [TedCatSmoke.Win]::FindVisible('TedCat.Hud'); ($null -ne $script:recordFixHud) -or [bool]((Get-LogSince $Mark) -match 'recording could not start|too small to record') } 20000)
        $failure = Wait-Log 'recording could not start: .*|too small to record' $Mark 200
        if ($failure) { throw "TedCat did not start recording: $failure" }
        Assert ($null -ne $script:recordFixHud) 'no recording pill (TedCat.Hud) appeared within 20 s'
        $line = Wait-Log 'starting region recording.*' $Mark 500
        if ($line) { Note $line }
    }

    # Clicks the recording pill and waits for "stopping the recording".
    function Stop-RecordFixRecording([int]$Mark) {
        $pill = [TedCatSmoke.Win]::FindVisible('TedCat.Hud')
        Assert ($null -ne $pill) 'the recording pill disappeared before it was clicked'
        $p = Center $pill
        Invoke-Input ([TedCatSmoke.Input]::Click($p[0], $p[1], 40)) 'the click on the pill'
        Assert ($null -ne (Wait-Log 'stopping the recording' $Mark 5000)) 'clicking the pill did not stop the recording'
    }

    # Waits until the recording is saved and returns the one new .mp4 (copied to the artifacts).
    function Wait-RecordFixSaved([int]$Mark, [string[]]$Existing, [string]$ArtifactName) {
        $saved = Wait-Log 'saved .* bytes|recording failed: .*' $Mark 30000
        Assert ($null -ne $saved) 'the recording was not finalized within 30 s'
        Note $saved
        Assert ($saved -notmatch 'recording failed') "TedCat could not save the recording: $saved"
        $new = @(Get-RecordFixVideos | Where-Object { $Existing -notcontains $_ })
        Assert ($new.Count -eq 1) "expected one new .mp4 in $VideosDir, found $($new.Count)"
        $file = Get-Item -LiteralPath $new[0]
        Note ("{0}: {1} bytes" -f $file.Name, $file.Length)
        Assert ($file.Length -gt 0) 'the recording is empty'
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $ArtifactsDir $ArtifactName) -Force
        return $file
    }

    # ffprobe's view of the video stream (codec, width, height, duration), or $null without ffprobe.
    function Get-RecordFixProbe([string]$Path) {
        $ffprobe = Find-FFprobe
        if (-not $ffprobe) { return $null }
        $json = Invoke-Native $ffprobe @('-v', 'error', '-show_entries', 'stream=codec_type,codec_name,width,height:format=duration', '-of', 'json', $Path)
        $info = $json | ConvertFrom-Json
        $video = @($info.streams | Where-Object { $_.codec_type -eq 'video' })
        Assert ($video.Count -eq 1) "expected one video stream, ffprobe says: $json"
        return [pscustomobject]@{ Codec = $video[0].codec_name; Width = [int]$video[0].width; Height = [int]$video[0].height; Duration = [double]$info.format.duration }
    }

    function Test-RecordFixColour([System.Drawing.Color]$Pixel, [int]$R, [int]$G, [int]$B, [int]$Tolerance) {
        return ([Math]::Abs($Pixel.R - $R) -le $Tolerance -and [Math]::Abs($Pixel.G - $G) -le $Tolerance -and [Math]::Abs($Pixel.B - $B) -le $Tolerance)
    }

    Invoke-Step 'recording-delay-load' {
        $tool = Find-RecordFixImportTool
        if (-not $tool) { Skip 'neither dumpbin nor llvm-readobj is available to read the import table' }
        Note "reading the imports of $Exe with $($tool.Kind) ($($tool.Path))"
        if ($tool.Kind -eq 'dumpbin') {
            $text = Invoke-Native $tool.Path @('/nologo', '/imports', $Exe)
            # Normal imports come first; delay-load imports follow their own heading.
            $at = $text.IndexOf('following delay load imports')
            $normalText = if ($at -ge 0) { $text.Substring(0, $at) } else { $text }
            $delayText = if ($at -ge 0) { $text.Substring($at) } else { '' }
            $pattern = '(?im)^\s+([A-Za-z0-9_.\-]+\.dll)\s*$'
            $normal = @([regex]::Matches($normalText, $pattern) | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() })
            $delayed = @([regex]::Matches($delayText, $pattern) | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() })
        } else {
            $text = Invoke-Native $tool.Path @('--coff-imports', $Exe)
            $normal = @([regex]::Matches($text, '(?m)^Import \{\s*Name: (\S+)') | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() })
            $delayed = @([regex]::Matches($text, '(?m)^DelayImport \{\s*Name: (\S+)') | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() })
        }
        Note ("normal imports: {0}" -f ($normal -join ', '))
        Note ("delay-load imports: {0}" -f $(if ($delayed.Count) { $delayed -join ', ' } else { 'none' }))
        Assert ($normal.Count -gt 0) "could not read any import from the $($tool.Kind) output: $text"
        foreach ($dll in @('mfplat.dll', 'mfreadwrite.dll')) {
            Assert ($normal -notcontains $dll) "$dll is a normal import: TedCat.exe would not start on Windows N without the Media Feature Pack"
            Assert ($delayed -contains $dll) "$dll is not a delay-load import of TedCat.exe"
        }
    }

    Invoke-Step 'recording-downscale' {
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $w = 480; $h = 320
        Assert (($canvas[2] - $canvas[0]) -ge $w + 60 -and ($canvas[3] - $canvas[1]) -ge $h + 60) "the canvas is too small for a ${w}x${h} region ($($canvas -join ','))"
        $x1 = $canvas[0] + 30; $y1 = $canvas[1] + 30
        # Half size: 480x320 -> 240x160.
        $env:TEDCAT_TEST_MAX_VIDEO_SIZE = '240x240'
        try { $mark = Start-RecordFixTedCat } finally { Remove-Item -LiteralPath Env:\TEDCAT_TEST_MAX_VIDEO_SIZE -ErrorAction SilentlyContinue }
        try {
            $existing = Get-RecordFixVideos
            Start-RecordFixRecording $x1 $y1 $w $h $mark
            Start-Sleep -Seconds $RecordSeconds
            Stop-RecordFixRecording $mark
            $file = Wait-RecordFixSaved $mark $existing 'downscaled.mp4'

            $limit = Wait-Log 'TEDCAT_TEST_MAX_VIDEO_SIZE is set.*' $mark 100
            $plan = Wait-Log "starting region recording .*: ${w}x${h} encoded as 240x160 at" $mark 100
            $scaler = Wait-Log "scaling the ${w}x${h} region to 240x160 on the (GPU|CPU).*" $mark 100
            Note "log: $(if ($limit) { $limit } else { '(no test limit line)' })"
            Note "log: $(if ($scaler) { $scaler } else { '(no scaling line)' })"
            Assert ($null -ne $plan) "TedCat did not plan to encode the ${w}x${h} region as 240x160"
            Assert ($null -ne $scaler) 'TedCat did not log how it scales the region'

            $probe = Get-RecordFixProbe $file.FullName
            if (-not $probe) {
                Note 'ffprobe not available: size and picture checks SKIPPED'
            } else {
                Note ("ffprobe: video {0} {1}x{2}, duration {3:N2} s" -f $probe.Codec, $probe.Width, $probe.Height, $probe.Duration)
                Assert ($probe.Codec -eq 'h264') "video codec is $($probe.Codec), expected h264"
                Assert ($probe.Width -eq 240 -and $probe.Height -eq 160) "video is $($probe.Width)x$($probe.Height), expected the scaled 240x160"
                Assert ($probe.Duration -ge ($RecordSeconds - 0.5)) "duration $($probe.Duration) s is shorter than the ~$RecordSeconds s recorded"

                # The scaled picture must still be the region: canvas green in
                # the lower right, the ticker's black background top left.
                $ffmpeg = Join-Path (Split-Path (Find-FFprobe)) 'ffmpeg.exe'
                if (-not (Test-Path -LiteralPath $ffmpeg)) {
                    Note 'ffmpeg not available: picture check SKIPPED'
                } else {
                    $png = Join-Path $ArtifactsDir 'downscaled-frame.png'
                    Remove-Item -LiteralPath $png -Force -ErrorAction SilentlyContinue
                    $out = Invoke-Native $ffmpeg @('-v', 'error', '-y', '-ss', '1', '-i', $file.FullName, '-frames:v', '1', $png)
                    Assert (Test-Path -LiteralPath $png) "ffmpeg could not extract a frame: $out"
                    $bitmap = New-Object System.Drawing.Bitmap $png
                    try {
                        $size = '{0}x{1}' -f $bitmap.Width, $bitmap.Height
                        # Region (360, 240) = canvas (390, 270): plain canvas.
                        $green = $bitmap.GetPixel(180, 120)
                        # Region (180, 46) = ticker (150, 16): its black background, right of the text.
                        $black = $bitmap.GetPixel(90, 23)
                    } finally { $bitmap.Dispose() }
                    $c = [TedCatSmoke.SmokeForm]::CanvasColor
                    Note ("frame at 1 s: {0}; pixel (180,120) {1},{2},{3} (canvas {4},{5},{6}); pixel (90,23) {7},{8},{9} (ticker background black)" -f $size, $green.R, $green.G, $green.B, $c.R, $c.G, $c.B, $black.R, $black.G, $black.B)
                    Assert ($size -eq '240x160') "the extracted frame is $size"
                    Assert (Test-RecordFixColour $green $c.R $c.G $c.B 32) 'the scaled video does not show the canvas where it should (black, garbage or the wrong area)'
                    Assert (Test-RecordFixColour $black 0 0 0 48) 'the scaled video does not show the ticker where it should (wrong area or scale)'
                }
            }
            $card = Wait-Until { $null -ne [TedCatSmoke.Win]::FindVisible('TedCat.Card') } 3000
            Note ("saved card shown: {0}" -f $card)
            Assert-TedRunning
        } finally {
            Stop-RecordFixTedCat
        }
    }

    Invoke-Step 'recording-display-change' {
        $original = [TedCatSmokeRecordFix.Display]::Current()
        Assert ($null -ne $original) 'EnumDisplaySettings could not read the current display mode'
        Note ("display mode: {0}; modes offered: {1}" -f $original, ([TedCatSmokeRecordFix.Display]::All() -join '; '))
        $other = [TedCatSmokeRecordFix.Display]::FindOther($original)
        if (-not $other) { Skip "the display offers no other resolution than $original" }
        $test = [TedCatSmokeRecordFix.Display]::Apply($other, $true)
        if ($test -ne 0) { Skip "Windows would not switch the display to $other (CDS_TEST returned $test)" }

        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $w = 240; $h = 160
        $x1 = $canvas[0] + 30; $y1 = $canvas[1] + 30
        $bounds = [TedCatSmokeRecordFix.Display]::FormBounds([TedCatSmoke.Ui]::Form)
        $mark = Start-RecordFixTedCat
        try {
            $existing = Get-RecordFixVideos
            Start-RecordFixRecording $x1 $y1 $w $h $mark
            Start-Sleep -Milliseconds 1500
            $changed = $false
            try {
                $result = [TedCatSmokeRecordFix.Display]::Apply($other, $false)
                $changed = ($result -eq 0)
                Note ("switched the display from {0} to {1}: result {2}, screen now {3}" -f $original, $other, $result, [TedCatSmokeRecordFix.Display]::ScreenSize())
                Assert $changed "ChangeDisplaySettingsEx to $other failed with $result"
                $interrupted = Wait-Log 'recording interrupted: .*' $mark 15000
                $detail = Wait-Log 'the display changed from .*' $mark 100
                Note "log: $(if ($interrupted) { $interrupted } else { '(no interruption within 15 s)' })"
                if ($detail) { Note "log: $detail" }
            } finally {
                # Back to the original mode whatever happened, then the test window where it was.
                if ($changed -or ([TedCatSmokeRecordFix.Display]::ScreenSize() -ne ('{0}x{1}' -f $original.Width, $original.Height))) {
                    $back = [TedCatSmokeRecordFix.Display]::Apply($original, $false)
                    if ($back -ne 0) { $back = [TedCatSmokeRecordFix.Display]::Reset() }
                    Start-Sleep -Milliseconds 1000
                    Note ("restored the display to {0}: result {1}, screen now {2}" -f $original, $back, [TedCatSmokeRecordFix.Display]::ScreenSize())
                }
                [TedCatSmokeRecordFix.Display]::SetFormBounds([TedCatSmoke.Ui]::Form, $bounds)
            }
            Assert ([TedCatSmokeRecordFix.Display]::ScreenSize() -eq ('{0}x{1}' -f $original.Width, $original.Height)) "the display was not restored to $original (now $([TedCatSmokeRecordFix.Display]::ScreenSize()))"
            Assert ($null -ne $interrupted) 'TedCat did not interrupt the recording when the display resolution changed'
            Assert ($interrupted -match 'The display resolution changed during the recording') "TedCat interrupted the recording for another reason: $interrupted"
            $stopLine = Wait-Log 'stopping the recording' $mark 5000
            Assert ($null -ne $stopLine) 'TedCat did not stop the interrupted recording on its own'

            $file = Wait-RecordFixSaved $mark $existing 'display-change.mp4'
            $probe = Get-RecordFixProbe $file.FullName
            if (-not $probe) {
                Note 'ffprobe not available: stream checks SKIPPED'
            } else {
                Note ("ffprobe: video {0} {1}x{2}, duration {3:N2} s" -f $probe.Codec, $probe.Width, $probe.Height, $probe.Duration)
                Assert ($probe.Codec -eq 'h264') "video codec is $($probe.Codec), expected h264"
                Assert ($probe.Width -eq $w -and $probe.Height -eq $h) "video is $($probe.Width)x$($probe.Height), expected ${w}x${h}"
                Assert ($probe.Duration -ge 1.0) "only $($probe.Duration) s were kept of the ~1.5 s recorded before the change"
            }
            Assert-TedRunning
        } finally {
            Stop-RecordFixTedCat
        }
    }

    # -----------------------------------------------------------------------
    # UI fixes (ci/fix-ui): foreign WM_APP messages, quitting while a
    # recording is saved, and the end of the session with a recording.
    # Each step starts its own TedCat and leaves it stopped.
    #
    #   foreign-app-messages        WM_APP+1..3 with junk WPARAM/LPARAM posted
    #                               to TedCat.Main (exactly what an
    #                               HWND_BROADCAST delivers to it) and to the
    #                               message-only TedCat.Msg are ignored; TedCat
    #                               survives and a screenshot still works.
    #   quit-while-saving           with the recorder thread suspended, the
    #                               quit waits for the save: a press pending
    #                               when the quit starts is handed back, and a
    #                               press-and-hold drag during the wait passes
    #                               straight through; the file is complete.
    #   session-end-saves-recording WM_QUERYENDSESSION starts the save at once
    #                               and sets a shutdown block reason; when
    #                               WM_ENDSESSION returns (and TedCat is killed
    #                               like Windows does) the MP4 is complete.
    #   session-end-during-quit     a WM_ENDSESSION that arrives while a quit
    #                               is still saving waits for that save.

    $uiFixHelpers = @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace TedCatSmokeUiFix
{
    public static class Native
    {
        [DllImport("user32.dll", SetLastError = true)]
        public static extern bool PostMessage(IntPtr hwnd, uint msg, UIntPtr wparam, IntPtr lparam);
        [DllImport("user32.dll", SetLastError = true)]
        public static extern IntPtr SendMessageTimeout(IntPtr hwnd, uint msg, UIntPtr wparam, IntPtr lparam, uint flags, uint timeout, out UIntPtr result);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern bool ShutdownBlockReasonQuery(IntPtr hwnd, StringBuilder buffer, ref uint size);
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern IntPtr OpenThread(uint access, bool inherit, uint id);
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern uint SuspendThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern uint ResumeThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool CloseHandle(IntPtr handle);
        [DllImport("kernel32.dll")]
        public static extern int GetThreadDescription(IntPtr thread, out IntPtr description);
        [DllImport("kernel32.dll")]
        public static extern IntPtr LocalFree(IntPtr memory);
    }

    /// One SendMessageTimeout on a background thread, so the script can see
    /// whether the receiver has answered yet.
    public class PendingSend
    {
        public volatile bool Done;
        public bool Ok;
        public long Result;
        public int Error;
        public long ElapsedMs;
        readonly Thread thread;

        public PendingSend(IntPtr hwnd, uint msg, ulong wparam, long lparam, uint timeoutMs)
        {
            thread = new Thread(delegate()
            {
                Stopwatch watch = Stopwatch.StartNew();
                UIntPtr result;
                IntPtr ok = Native.SendMessageTimeout(hwnd, msg, new UIntPtr(wparam), new IntPtr(lparam), 0, timeoutMs, out result);
                Error = Marshal.GetLastWin32Error();
                ElapsedMs = watch.ElapsedMilliseconds;
                Result = (long)result.ToUInt64();
                Ok = ok != IntPtr.Zero;
                Done = true;
            });
            thread.IsBackground = true;
            thread.Start();
        }

        public bool Wait(int timeoutMs) { return thread.Join(timeoutMs); }

        public override string ToString()
        {
            if (!Done) return "no answer yet";
            if (Ok) return "answered " + Result + " after " + ElapsedMs + " ms";
            return "failed (error " + Error + ") after " + ElapsedMs + " ms";
        }
    }

    public static class Session
    {
        const uint THREAD_SUSPEND_RESUME = 0x0002;
        const uint THREAD_QUERY_LIMITED_INFORMATION = 0x0800;

        /// The message-only window of class `cls` (parent HWND_MESSAGE), or zero.
        public static IntPtr MessageWindow(string cls)
        {
            return Native.FindWindowEx(new IntPtr(-3), IntPtr.Zero, cls, null);
        }

        /// PostMessage; null on success, otherwise the error.
        public static string Post(IntPtr hwnd, uint msg, ulong wparam, long lparam)
        {
            if (Native.PostMessage(hwnd, msg, new UIntPtr(wparam), new IntPtr(lparam))) return null;
            return "PostMessage failed with error " + Marshal.GetLastWin32Error();
        }

        public static PendingSend SendAsync(IntPtr hwnd, uint msg, ulong wparam, long lparam, uint timeoutMs)
        {
            return new PendingSend(hwnd, msg, wparam, lparam, timeoutMs);
        }

        public static PendingSend Send(IntPtr hwnd, uint msg, ulong wparam, long lparam, uint timeoutMs)
        {
            PendingSend send = new PendingSend(hwnd, msg, wparam, lparam, timeoutMs);
            send.Wait((int)timeoutMs + 5000);
            return send;
        }

        /// The shutdown block reason of a window, as far as this process can read it.
        public static string BlockReason(IntPtr hwnd)
        {
            uint size = 0;
            if (!Native.ShutdownBlockReasonQuery(hwnd, null, ref size)) return "(none readable, error " + Marshal.GetLastWin32Error() + ")";
            StringBuilder text = new StringBuilder((int)size + 1);
            size = (uint)text.Capacity;
            if (!Native.ShutdownBlockReasonQuery(hwnd, text, ref size)) return "(unreadable, error " + Marshal.GetLastWin32Error() + ")";
            return text.ToString();
        }

        static string Describe(IntPtr thread)
        {
            IntPtr text;
            if (Native.GetThreadDescription(thread, out text) < 0 || text == IntPtr.Zero) return null;
            string name = Marshal.PtrToStringUni(text);
            Native.LocalFree(text);
            return name;
        }

        public static string ThreadNames(int pid)
        {
            List<string> names = new List<string>();
            foreach (ProcessThread t in Process.GetProcessById(pid).Threads)
            {
                IntPtr h = Native.OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, (uint)t.Id);
                if (h == IntPtr.Zero) continue;
                string name = Describe(h);
                Native.CloseHandle(h);
                if (!string.IsNullOrEmpty(name)) names.Add(name);
            }
            return string.Join(", ", names.ToArray());
        }

        /// Suspends the thread of process `pid` whose description is exactly
        /// `name`. Returns its handle (give it to Resume), or zero.
        public static IntPtr SuspendNamed(int pid, string name)
        {
            foreach (ProcessThread t in Process.GetProcessById(pid).Threads)
            {
                IntPtr h = Native.OpenThread(THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION, false, (uint)t.Id);
                if (h == IntPtr.Zero) continue;
                if (Describe(h) == name && Native.SuspendThread(h) != 0xFFFFFFFF) return h;
                Native.CloseHandle(h);
            }
            return IntPtr.Zero;
        }

        /// Resumes and closes a thread from SuspendNamed (zero is ignored).
        public static bool Resume(IntPtr thread)
        {
            if (thread == IntPtr.Zero) return false;
            bool ok = Native.ResumeThread(thread) != 0xFFFFFFFF;
            Native.CloseHandle(thread);
            return ok;
        }

        /// The top-level boxes of an MP4 file ("ftyp mdat moov"), with
        /// "(truncated)" when a box runs past the end of the file.
        public static string Mp4Boxes(string path)
        {
            try
            {
                using (FileStream file = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete))
                {
                    long length = file.Length, offset = 0;
                    List<string> boxes = new List<string>();
                    byte[] header = new byte[16];
                    while (offset + 8 <= length)
                    {
                        file.Seek(offset, SeekOrigin.Begin);
                        int read = file.Read(header, 0, 16);
                        if (read < 8) break;
                        long size = ((long)header[0] << 24) | ((long)header[1] << 16) | ((long)header[2] << 8) | header[3];
                        boxes.Add(Encoding.ASCII.GetString(header, 4, 4));
                        if (size == 1 && read >= 16)
                        {
                            size = 0;
                            for (int i = 8; i < 16; i++) size = (size << 8) | header[i];
                        }
                        else if (size == 0)
                        {
                            size = length - offset;
                        }
                        if (size < 8) { boxes.Add("(bad size)"); break; }
                        if (offset + size > length) { boxes.Add("(truncated)"); break; }
                        offset += size;
                    }
                    return string.Join(" ", boxes.ToArray());
                }
            }
            catch (Exception e)
            {
                return "error: " + e.Message;
            }
        }
    }
}
'@
    Add-Type -TypeDefinition $uiFixHelpers -Language CSharp

    $WM_CLOSE = [uint32]0x0010
    $WM_QUERYENDSESSION = [uint32]0x0011
    $WM_ENDSESSION = [uint32]0x0016
    $ENDSESSION_LOGOFF = [int64]2147483648   # 0x80000000
    $RecorderThread = 'TedCat recorder'

    # A fresh TedCat with the known settings; returns its TedCat.Main window.
    function Start-UiFixTedCat([string]$Why) {
        Get-Process -Name TedCat -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
        Write-Settings
        $mark = Get-LogMark
        $script:Ted = Start-TedCat
        Assert ($null -ne (Wait-Log 'input hook status: Active' $mark 15000)) "TedCat did not start ($Why)"
        $main = [TedCatSmoke.Win]::Find('TedCat.Main')
        Assert ($main -ne [IntPtr]::Zero) 'no TedCat.Main window'
        return $main
    }

    # Shift + hold + drag on the canvas until the recording pill shows; returns the output path.
    function Start-UiFixRecording {
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $x1 = $canvas[0] + 30; $y1 = $canvas[1] + 30
        $mark = Get-LogMark
        Invoke-Input ([TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $false)) 'Shift down'
        try {
            Start-Sleep -Milliseconds 80
            Invoke-HoldDrag $x1 $y1 ($x1 + 400) ($y1 + 300) 600
            Start-Sleep -Milliseconds 80
        } finally {
            [void][TedCatSmoke.Input]::Key($VK_SHIFT, $SC_SHIFT, $true)
        }
        $line = Wait-Log 'starting recording .* to .+\.mp4\s*$|recording could not start: .*|too small to record' $mark 20000
        Assert ($null -ne $line) 'TedCat did not start a recording within 20 s'
        Assert ($line -match ' to (.+\.mp4)\s*$') "TedCat did not start recording: $line"
        $path = $Matches[1]
        Assert (Wait-Until { $null -ne [TedCatSmoke.Win]::FindVisible('TedCat.Hud') } 10000) 'no recording pill (TedCat.Hud) appeared'
        return $path
    }

    # Suspends TedCat's recorder thread so a save cannot finish until Resume.
    function Suspend-UiFixRecorder {
        $thread = [TedCatSmokeUiFix.Session]::SuspendNamed($script:Ted.Id, $RecorderThread)
        if ($thread -eq [IntPtr]::Zero) {
            Skip "could not find and suspend TedCat's '$RecorderThread' thread (threads: $([TedCatSmokeUiFix.Session]::ThreadNames($script:Ted.Id)))"
        }
        Note "suspended TedCat's '$RecorderThread' thread: the save cannot finish until it is resumed"
        return $thread
    }

    # The MP4 must be finalized (moov box present, nothing truncated).
    function Assert-UiFixMp4Complete([string]$Path, [string]$When) {
        Assert (Test-Path -LiteralPath $Path) "the recording $Path does not exist"
        $boxes = [TedCatSmokeUiFix.Session]::Mp4Boxes($Path)
        Note ("{0}: {1} bytes, boxes [{2}]" -f (Split-Path -Leaf $Path), (Get-Item -LiteralPath $Path).Length, $boxes)
        Assert ($boxes -match '\bmoov\b' -and $boxes -notmatch 'truncated|bad size|error') "the recording was not finalized $When (boxes: $boxes)"
        $ffprobe = Find-FFprobe
        if ($ffprobe) {
            $duration = (Invoke-Native $ffprobe @('-v', 'error', '-show_entries', 'format=duration', '-of', 'default=nw=1:nk=1', $Path)).Trim()
            Note "ffprobe duration: $duration s"
            Assert ($duration -match '^\d+(\.\d+)?$' -and [double]$duration -gt 0) "ffprobe cannot read the recording: $duration"
        }
        Copy-Item -LiteralPath $Path -Destination $ArtifactsDir -ErrorAction SilentlyContinue
    }

    # Press on the knob, run $DuringHold while the button is down, wait past the
    # hold time, drag the knob by $Dx, release. Returns "tagged sequence|moved".
    function Invoke-UiFixKnobHold([int]$Dx, [scriptblock]$DuringHold = $null) {
        [TedCatSmoke.Ui]::ResetKnob()
        Start-Sleep -Milliseconds 200
        [TedCatSmoke.Ui]::ClearEvents()
        $before = [TedCatSmoke.Ui]::Rect('Knob')
        $a = Center $before
        Invoke-Input ([TedCatSmoke.Input]::Move($a[0], $a[1])) 'the move to the knob'
        Start-Sleep -Milliseconds 60
        Invoke-Input ([TedCatSmoke.Input]::Down($a[0], $a[1])) 'the press on the knob'
        try {
            if ($DuringHold) { & $DuringHold | Out-Host }
            Start-Sleep -Milliseconds 700   # well past the 350 ms hold
            Invoke-Input ([TedCatSmoke.Input]::MovePath($a[0], $a[1], $a[0] + $Dx, $a[1], 15, 15)) 'the drag'
            Start-Sleep -Milliseconds 150
        } finally {
            if (-not [TedCatSmoke.Input]::Up($a[0] + $Dx, $a[1])) { Write-Host "    (release failed: $([TedCatSmoke.Input]::LastError))" }
        }
        Start-Sleep -Milliseconds 400
        $after = [TedCatSmoke.Ui]::Rect('Knob')
        return ('{0}|{1}' -f (Get-TaggedSequence 'Knob'), ($after[0] - $before[0]))
    }

    Invoke-Step 'foreign-app-messages' {
        $main = Start-UiFixTedCat 'foreign-app-messages'
        $msg = [TedCatSmokeUiFix.Session]::MessageWindow('TedCat.Msg')
        $topLevel = @([TedCatSmoke.Win]::Windows('TedCat.Msg'))
        Note ("TedCat.Main {0}; message-only TedCat.Msg {1}; TedCat.Msg among top-level windows: {2}" -f $main, $msg, $topLevel.Count)
        Assert ($msg -ne [IntPtr]::Zero) 'no message-only TedCat.Msg window'
        Assert ($topLevel.Count -eq 0) 'TedCat.Msg is a top-level window, so other processes broadcasts reach it'
        $mark = Get-LogMark
        $posted = 0
        foreach ($n in 1..3) {
            foreach ($target in @($main, $msg)) {
                foreach ($junk in @(@(0, 0x41414141), @(0x1234, 0x7FFF00001000), @(1, 8))) {
                    $err = [TedCatSmokeUiFix.Session]::Post($target, [uint32](0x8000 + $n), [uint64]$junk[0], [int64]$junk[1])
                    Assert ($null -eq $err) "posting WM_APP+$n failed: $err"
                    $posted++
                }
            }
        }
        Start-Sleep -Milliseconds 800
        Assert-TedRunning
        $onMain = Wait-Log 'ignored WM_APP\+\d on the main window.*' $mark 2000
        $onMsg = Wait-Log 'ignored WM_APP\+\d on the message window.*' $mark 2000
        Note "posted $posted WM_APP+1..3 messages with junk WPARAM/LPARAM; TedCat is still running"
        Note "log: $(if ($onMain) { $onMain } else { '(nothing about the main window)' })"
        Note "log: $(if ($onMsg) { $onMsg } else { '(nothing about the message window)' })"
        Assert ($null -ne $onMain) 'TedCat did not report ignoring the junk posted to TedCat.Main'
        Assert ($null -ne $onMsg) 'TedCat did not report ignoring the junk posted to TedCat.Msg'

        # TedCat's own posts (with the cookie) still get through: a screenshot
        # needs the hook's WM_APP_GESTURE messages and the UI thread.
        Assert ([TedCatSmoke.Clip]::Clear()) 'could not clear the clipboard'
        $canvas = [TedCatSmoke.Ui]::Rect('Canvas')
        $x1 = $canvas[0] + 40; $y1 = $canvas[1] + 40
        $mark = Get-LogMark
        Invoke-HoldDrag $x1 $y1 ($x1 + 200) ($y1 + 120) 600
        $hasImage = Wait-Until { Clipboard-HasImage } 5000
        $shot = Wait-Log 'screenshot \d+x\d+ copied' $mark 2000
        Note "screenshot after the junk: $(if ($shot) { $shot } else { '(none)' }); clipboard image: $hasImage"
        Assert ($hasImage -and $null -ne $shot) 'the hold-and-drag screenshot no longer works after the junk messages'
        Assert-TedRunning
        Stop-Process -Id $script:Ted.Id -Force -ErrorAction SilentlyContinue
        [void]$script:Ted.WaitForExit(10000)
    }

    Invoke-Step 'quit-while-saving' {
        $main = Start-UiFixTedCat 'quit-while-saving'
        $path = Start-UiFixRecording
        Note "recording to $path"
        Start-Sleep -Milliseconds 1000
        $recorder = Suspend-UiFixRecorder
        $mark = Get-LogMark
        $dx = 150
        try {
            # 1. Pressed just before Quit: still undecided when the gesture is
            #    switched off, so it must come back as TedCat's replay.
            $script:closeError = 'not sent'
            $pending = Invoke-UiFixKnobHold $dx { $script:closeError = [TedCatSmokeUiFix.Session]::Post($main, $WM_CLOSE, 0, 0) }
            Assert ($null -eq $script:closeError) "posting WM_CLOSE failed: $($script:closeError)"
            $quitting = Wait-Log 'finalizing the recording before quitting' $mark 5000
            $off = Wait-Log 'TedCat is quitting; presses are no longer held' $mark 100
            # 2. Pressed during the save: never held, passes straight through.
            $during = Invoke-UiFixKnobHold $dx
            $alive = -not $script:Ted.HasExited
        } finally {
            [void][TedCatSmokeUiFix.Session]::Resume($recorder)
        }
        Note "log: $(if ($off) { $off } else { '(no gesture-off line)' })"
        Note "log: $(if ($quitting) { $quitting } else { '(no finalizing line)' })"
        Note "press pending when Quit started: knob got '$(($pending -split '\|')[0])', moved $(($pending -split '\|')[1]) px (expected $dx)"
        Note "press-and-hold drag during the save: knob got '$(($during -split '\|')[0])', moved $(($during -split '\|')[1]) px (expected $dx)"
        Note "TedCat still running (saving) during both presses: $alive"
        Assert ($null -ne $quitting) 'TedCat did not start finalizing the recording on Quit'
        Assert $alive 'TedCat exited before the save was released, so the presses did not happen during the save'
        Assert (($pending -split '\|')[0] -eq 'DOWN:TCAT UP:0x0') "the press pending at Quit was not handed back (expected 'DOWN:TCAT UP:0x0', got '$(($pending -split '\|')[0])')"
        Assert (($during -split '\|')[0] -eq 'DOWN:0x0 UP:0x0') "a press-and-hold drag during the save was held or swallowed (expected 'DOWN:0x0 UP:0x0', got '$(($during -split '\|')[0])')"
        foreach ($result in @($pending, $during)) {
            Assert ([Math]::Abs([int](($result -split '\|')[1]) - $dx) -le 3) "the knob did not follow the drag ($result)"
        }

        $exited = $script:Ted.WaitForExit(30000)
        $saved = Wait-Log 'saved .* before quitting|recording failed while quitting.*|did not finish in time' $mark 2000
        Note ("after the save was released: TedCat exited {0} (code {1}); log: {2}" -f $exited, $(if ($exited) { $script:Ted.ExitCode } else { '-' }), $(if ($saved) { $saved } else { '(none)' }))
        Assert $exited 'TedCat did not exit within 30 s after the save was released'
        Assert ($script:Ted.ExitCode -eq 0) ('TedCat exited with code 0x{0:X8}' -f $script:Ted.ExitCode)
        Assert ($saved -match 'saved .* before quitting') "the recording was not saved on Quit: $saved"
        Assert-UiFixMp4Complete $path 'on Quit'
    }

    Invoke-Step 'session-end-saves-recording' {
        $main = Start-UiFixTedCat 'session-end-saves-recording'
        $path = Start-UiFixRecording
        Note "recording to $path"
        Start-Sleep -Seconds 2
        $mark = Get-LogMark
        $query = [TedCatSmokeUiFix.Session]::Send($main, $WM_QUERYENDSESSION, 0, $ENDSESSION_LOGOFF, 10000)
        $stopping = Wait-Log 'stopping the recording' $mark 3000
        $blocked = Wait-Log 'shutdown block reason set.*|could not set a shutdown block reason.*' $mark 1000
        Note "WM_QUERYENDSESSION: $query"
        Note "log: $(if ($stopping) { $stopping } else { '(the recording was not stopped)' })"
        Note "log: $(if ($blocked) { $blocked } else { '(no shutdown block reason line)' }); reason read from here: $([TedCatSmokeUiFix.Session]::BlockReason($main))"
        Assert ($query.Ok -and $query.Result -eq 1) "TedCat did not answer WM_QUERYENDSESSION with TRUE ($query)"
        Assert ($null -ne $stopping) 'WM_QUERYENDSESSION did not start finalizing the recording'
        Assert ($blocked -match 'shutdown block reason set') "no shutdown block reason while the recording is saved: $blocked"

        $end = [TedCatSmokeUiFix.Session]::Send($main, $WM_ENDSESSION, 1, $ENDSESSION_LOGOFF, 40000)
        # Windows may end the process as soon as WM_ENDSESSION returns: do exactly that.
        Stop-Process -Id $script:Ted.Id -Force -ErrorAction SilentlyContinue
        [void]$script:Ted.WaitForExit(10000)
        $saved = Wait-Log 'recording saved: .*|saved .* bytes.*' $mark 500
        Note "WM_ENDSESSION: $end; TedCat then ended at once, as Windows would"
        Note "log: $(if ($saved) { $saved } else { '(no saved line)' })"
        Assert $end.Ok "WM_ENDSESSION did not return within 40 s ($end)"
        Assert-UiFixMp4Complete $path 'when WM_ENDSESSION returned'
    }

    Invoke-Step 'session-end-during-quit' {
        $main = Start-UiFixTedCat 'session-end-during-quit'
        $path = Start-UiFixRecording
        Note "recording to $path"
        Start-Sleep -Milliseconds 1000
        $recorder = Suspend-UiFixRecorder
        $mark = Get-LogMark
        $end = $null
        try {
            $closeError = [TedCatSmokeUiFix.Session]::Post($main, $WM_CLOSE, 0, 0)
            Assert ($null -eq $closeError) "posting WM_CLOSE failed: $closeError"
            $quitting = Wait-Log 'finalizing the recording before quitting' $mark 5000
            Assert ($null -ne $quitting) 'TedCat did not start finalizing the recording on Quit'
            $query = [TedCatSmokeUiFix.Session]::Send($main, $WM_QUERYENDSESSION, 0, $ENDSESSION_LOGOFF, 10000)
            $blocked = Wait-Log 'shutdown block reason set.*|could not set a shutdown block reason.*' $mark 1000
            Note "quit in progress; WM_QUERYENDSESSION: $query"
            Note "log: $(if ($blocked) { $blocked } else { '(no shutdown block reason line)' }); reason read from here: $([TedCatSmokeUiFix.Session]::BlockReason($main))"
            Assert ($query.Ok -and $query.Result -eq 1) "TedCat did not answer WM_QUERYENDSESSION with TRUE ($query)"
            Assert ($blocked -match 'shutdown block reason set') "no shutdown block reason while the quit saves: $blocked"
            $end = [TedCatSmokeUiFix.Session]::SendAsync($main, $WM_ENDSESSION, 1, $ENDSESSION_LOGOFF, 40000)
            Start-Sleep -Milliseconds 1500
            $early = $end.Done
            Note "WM_ENDSESSION while the save is blocked: $end"
            Assert (-not $early) "WM_ENDSESSION returned ($end) while the quit was still saving the recording"
        } finally {
            [void][TedCatSmokeUiFix.Session]::Resume($recorder)
        }
        Assert ($end.Wait(35000)) 'WM_ENDSESSION did not return within 35 s after the save was released'
        # Windows may end the process as soon as WM_ENDSESSION returns: do exactly that.
        Stop-Process -Id $script:Ted.Id -Force -ErrorAction SilentlyContinue
        [void]$script:Ted.WaitForExit(10000)
        $saved = Wait-Log 'saved .* before quitting|recording saved: .*' $mark 500
        Note "after the save was released, WM_ENDSESSION: $end; TedCat then ended at once, as Windows would"
        Note "log: $(if ($saved) { $saved } else { '(no saved line)' })"
        Assert $end.Ok "WM_ENDSESSION failed ($end)"
        Assert-UiFixMp4Complete $path 'when WM_ENDSESSION returned during the quit'
    }
} catch {
    Write-Host "Smoke test aborted: $($_.Exception.Message) (script line $($_.InvocationInfo.ScriptLineNumber))"
    $script:Results.Add([pscustomobject]@{ Step = 'script'; Result = 'failed'; Evidence = $_.Exception.Message })
    Save-Desktop 'aborted'
} finally {
    if ($script:Ted -and -not $script:Ted.HasExited) {
        Stop-Process -Id $script:Ted.Id -Force -ErrorAction SilentlyContinue
    }
    Get-Process -Name TedCat -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    [TedCatSmoke.Ui]::Close()
    Save-Desktop 'final'

    Write-Host ''
    Write-Host '=== TedCat log ==='
    $log = Read-TedLog
    Write-Host $log
    if (Test-Path -LiteralPath $LogPath) { Copy-Item -LiteralPath $LogPath -Destination $ArtifactsDir -Force }

    Write-Host ''
    Write-Host '=== Summary ==='
    $script:Results | Format-Table -AutoSize -Wrap | Out-String -Width 220 | Write-Host
    $script:Results | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $ArtifactsDir 'summary.json') -Encoding UTF8
    if ($env:GITHUB_STEP_SUMMARY) {
        $md = New-Object System.Text.StringBuilder
        [void]$md.AppendLine('### TedCat smoke test')
        [void]$md.AppendLine('')
        [void]$md.AppendLine('| Step | Result | Evidence |')
        [void]$md.AppendLine('|------|--------|----------|')
        foreach ($r in $script:Results) {
            [void]$md.AppendLine(('| {0} | {1} | {2} |' -f $r.Step, $r.Result.ToUpper(), ($r.Evidence -replace '\|', '/' -replace "`r?`n", ' ')))
        }
        Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $md.ToString() -Encoding UTF8
    }
    $failed = @($script:Results | Where-Object { $_.Result -eq 'failed' })
    if ($failed.Count -gt 0) {
        Write-Host ("FAILED: {0}" -f (($failed | ForEach-Object { $_.Step }) -join ', '))
        $exitCode = 1
    } else {
        Write-Host 'No step failed.'
    }
}
exit $exitCode
