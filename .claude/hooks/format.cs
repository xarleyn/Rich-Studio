#!/usr/bin/env -S dotnet run --file
#:property TargetFramework=net10.0
#:property Nullable=enable

// Claude Code hook (PostToolUse on Edit/Write): format the file the agent just changed.
// The .NET port of format.py: same rules, same silence.
//
// Never fails the tool call: a missing tool, a lint error or a timeout is left for `make check`.
// Reads one JSON object from stdin and uses tool_input.file_path:
//   *.py            ruff format -q, then ruff check -q --fix
//   *.js / *.mjs    npx --no-install eslint --fix, except a path under /vendor/ (\vendor\ on Windows)
//   anything else   nothing
// Each command gets 60 s and its output is discarded. The exit code is always 0.
//
// Run: dotnet run --file .claude/hooks/format.cs

using System.Diagnostics;
using System.Text;
using System.Text.Json;

const int TimeoutMs = 60_000;

var path = FilePathFromStdin();
var commands = new List<string[]>();
if (path.EndsWith(".py", StringComparison.Ordinal))
{
    commands.Add(["ruff", "format", "-q", path]);
    commands.Add(["ruff", "check", "-q", "--fix", path]);
}
else if ((path.EndsWith(".js", StringComparison.Ordinal) || path.EndsWith(".mjs", StringComparison.Ordinal))
         && !path.Contains("/vendor/", StringComparison.Ordinal)
         && !path.Contains("\\vendor\\", StringComparison.Ordinal))
{
    commands.Add(["npx", "--no-install", "eslint", "--fix", path]);
}

foreach (var command in commands)
{
    Run(command, TimeoutMs);
}

return 0;

// The path the agent just wrote; "" when stdin is not the hook payload.
static string FilePathFromStdin()
{
    try
    {
        using var stdin = Console.OpenStandardInput();
        using var buffer = new MemoryStream();
        stdin.CopyTo(buffer);
        var text = Encoding.UTF8.GetString(buffer.ToArray()).TrimStart('\uFEFF');
        using var document = JsonDocument.Parse(text);
        if (document.RootElement.ValueKind == JsonValueKind.Object
            && document.RootElement.TryGetProperty("tool_input", out var input)
            && input.ValueKind == JsonValueKind.Object
            && input.TryGetProperty("file_path", out var value)
            && value.ValueKind == JsonValueKind.String)
        {
            return value.GetString() ?? "";
        }
    }
    catch (Exception)
    {
        // not JSON, an unreadable stdin, anything else: the hook has nothing to do
    }

    return "";
}

// Run one formatter with a hard timeout; every failure is swallowed on purpose.
static void Run(string[] command, int timeoutMs)
{
    try
    {
        var start = new ProcessStartInfo
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
        };
        if (OperatingSystem.IsWindows())
        {
            // npx is npx.cmd, a batch file: CreateProcess cannot start it, only cmd.exe can.
            // Every argument is quoted; % still expands inside quotes, so such a path is skipped.
            var path = command[^1];
            if (path.Contains('%') || path.Contains('"') || path.Any(char.IsControl))
            {
                return;
            }

            // The program name stays bare: a quoted "npx" makes %~dp0 inside npx.cmd point at the current
            // directory, and npx then cannot find its own npm. Only the arguments are quoted.
            start.FileName = "cmd.exe";
            start.Arguments = "/d /s /c " + command[0] + " "
                              + string.Join(' ', command.Skip(1).Select(QuoteForCmd));
        }
        else
        {
            start.FileName = command[0];
            foreach (var argument in command.Skip(1))
            {
                start.ArgumentList.Add(argument);
            }
        }

        using var process = Process.Start(start);
        if (process is null)
        {
            return;
        }

        // Drain both pipes: a formatter that fills the buffer would otherwise hang until the timeout.
        var stdout = process.StandardOutput.ReadToEndAsync();
        var stderr = process.StandardError.ReadToEndAsync();
        if (!process.WaitForExit(timeoutMs))
        {
            process.Kill(entireProcessTree: true);
            process.WaitForExit(5_000);
        }

        Task.WaitAll([stdout, stderr], 5_000);
    }
    catch (Exception)
    {
        // missing ruff or npx, a killed process, a timeout: the hook stays quiet
    }
}

static string QuoteForCmd(string value) => "\"" + value + "\"";
