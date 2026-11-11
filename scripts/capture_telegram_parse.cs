// Record how the real Telegram parses rich HTML and Markdown.
//
// Sends every case from tests/telegram_parse/cases.json with sendRichMessage, saves the blocks Telegram
// answers with to tests/telegram_parse/captured.json and deletes the message right away. The preview
// parity test (npm run test:parity) then checks that render.js draws the source the way Telegram does.
// Needs the real bot: TG_BOT_TOKEN and a chat where it may post (PARSE_CHAT, default TG_PREVIEW_CHAT).
// Messages go out silently and live for a second. Run after a Bot API update or when adding a case:
//   dotnet run --file scripts/capture_telegram_parse.cs [case-name ...]

using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

Console.OutputEncoding = Encoding.UTF8;

var token = Environment.GetEnvironmentVariable("TG_BOT_TOKEN") ?? "";
var chat = Environment.GetEnvironmentVariable("PARSE_CHAT") ?? "";
if (string.IsNullOrEmpty(chat)) chat = Environment.GetEnvironmentVariable("TG_PREVIEW_CHAT") ?? "";
if (string.IsNullOrEmpty(token) || string.IsNullOrEmpty(chat))
{
    Console.WriteLine("нужны TG_BOT_TOKEN и PARSE_CHAT (или TG_PREVIEW_CHAT)");
    return 1;
}
var apiBase = Environment.GetEnvironmentVariable("TG_API_BASE");
if (string.IsNullOrEmpty(apiBase)) apiBase = "https://api.telegram.org";
var baseUrl = apiBase.TrimEnd('/') + "/bot" + token;
var only = args.ToHashSet(StringComparer.Ordinal);

var root = FindRepoRoot();
var directory = Path.Combine(root, "tests", "telegram_parse");
var casesPath = Path.Combine(directory, "cases.json");
var capturedPath = Path.Combine(directory, "captured.json");
var bundledPath = Path.Combine(root, "app", "bundled", "api.json");

JsonNode? specVersion;
JsonArray cases;
JsonObject captured;
try
{
    specVersion = JsonNode.Parse(File.ReadAllBytes(bundledPath))?["version"]?.DeepClone();
    cases = JsonNode.Parse(File.ReadAllBytes(casesPath)) as JsonArray
            ?? throw new JsonException("ожидался массив");
    captured = File.Exists(capturedPath)
        ? JsonNode.Parse(File.ReadAllBytes(capturedPath)) as JsonObject ?? new JsonObject()
        : new JsonObject();
}
catch (Exception error) when (error is IOException or JsonException or UnauthorizedAccessException
                                  or InvalidOperationException)
{
    Console.WriteLine($"не удалось прочитать данные кейсов: {error.Message}");
    return 1;
}

var selected = cases
    .OfType<JsonObject>()
    .Where(testCase => only.Count == 0
                       || (testCase["name"] is JsonValue nameValue
                           && nameValue.TryGetValue<string>(out var name)
                           && only.Contains(name)))
    .ToList();

using var client = new HttpClient { Timeout = TimeSpan.FromSeconds(30) };
var failed = 0;

foreach (var testCase in selected)
{
    var name = NodeText(testCase["name"]);
    var format = NodeText(testCase["format"]);
    var source = NodeText(testCase["source"]);
    var response = await CallAsync(
        "sendRichMessage",
        new JsonObject
        {
            ["chat_id"] = chat,
            ["rich_message"] = new JsonObject { [format] = source },
            ["disable_notification"] = true,
        });
    if (!IsOk(response))
    {
        failed++;
        Console.WriteLine($"{name}: Telegram отклонил: {PyText(response?["description"])}");
        captured[name] = new JsonObject
        {
            ["source"] = source,
            ["format"] = format,
            ["error"] = response?["description"]?.DeepClone(),
        };
        continue;
    }
    if (response?["result"] is not JsonObject message)
    {
        failed++;
        Console.WriteLine($"{name}: Telegram отклонил: в ответе нет result");
        captured[name] = new JsonObject
        {
            ["source"] = source,
            ["format"] = format,
            ["error"] = "в ответе нет result",
        };
        continue;
    }
    if (message["rich_message"]?["blocks"] is not JsonArray blocks)
    {
        failed++;
        Console.WriteLine($"{name}: Telegram отклонил: в ответе нет rich_message.blocks");
        captured[name] = new JsonObject
        {
            ["source"] = source,
            ["format"] = format,
            ["error"] = "в ответе нет rich_message.blocks",
        };
        continue;
    }
    var messageId = message["message_id"]?.DeepClone();
    var deleted = await CallAsync(
        "deleteMessage",
        new JsonObject { ["chat_id"] = chat, ["message_id"] = messageId });
    if (!IsOk(deleted))
        Console.WriteLine($"  не удалось удалить сообщение {NodeText(messageId)}: удалите его вручную");
    captured[name] = new JsonObject
    {
        ["format"] = format,
        ["source"] = source,
        ["blocks"] = blocks.DeepClone(),
        ["spec"] = specVersion?.DeepClone(),
        ["captured_at"] = DateTime.UtcNow.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
    };
    Console.WriteLine($"{name}: {blocks.Count} блоков");
    await Task.Delay(TimeSpan.FromSeconds(3)); // groups allow about 20 messages a minute
}

try
{
    File.WriteAllText(capturedPath, PyJson.Serialize(captured) + "\n", new UTF8Encoding(false));
}
catch (Exception error) when (error is IOException or UnauthorizedAccessException)
{
    Console.WriteLine($"не удалось записать tests/telegram_parse/captured.json: {error.Message}");
    return 1;
}
return failed == 0 ? 0 : 1;

static string FindRepoRoot()
{
    var current = new DirectoryInfo(Directory.GetCurrentDirectory());
    while (current is not null)
    {
        if (File.Exists(Path.Combine(current.FullName, "app", "bundled", "api.json")))
            return current.FullName;
        current = current.Parent;
    }
    throw new InvalidOperationException("не найден корень репозитория (app/bundled/api.json)");
}

static string NodeText(JsonNode? node) => node switch
{
    null => "",
    JsonValue value when value.TryGetValue<string>(out var text) => text,
    _ => node.ToJsonString(),
};

static string PyText(JsonNode? node) => node switch
{
    null => "None",
    JsonValue value when value.TryGetValue<string>(out var text) => text,
    JsonValue value when value.GetValueKind() == JsonValueKind.True => "True",
    JsonValue value when value.GetValueKind() == JsonValueKind.False => "False",
    _ => node.ToJsonString(),
};

static bool IsOk(JsonNode? response) =>
    response is JsonObject reply
    && reply["ok"] is JsonValue value
    && value.TryGetValue<bool>(out var ok)
    && ok;

async Task<JsonNode?> CallAsync(string method, JsonObject parameters)
{
    // never print the URL: it carries the token
    JsonNode? last = null;
    for (var attempt = 0; attempt < 3; attempt++)
    {
        JsonNode? parsed;
        try
        {
            using var content = new StringContent(parameters.ToJsonString(), Encoding.UTF8, "application/json");
            using var response = await client.PostAsync($"{baseUrl}/{method}", content);
            var body = await response.Content.ReadAsStringAsync();
            parsed = JsonNode.Parse(body);
        }
        catch (Exception error) when (error is HttpRequestException or TaskCanceledException
                                          or IOException or JsonException or InvalidOperationException
                                          or UriFormatException or ArgumentException)
        {
            return new JsonObject { ["ok"] = false, ["description"] = "нет ответа от Telegram" };
        }
        last = parsed;
        if (parsed is not JsonObject reply)
            return new JsonObject { ["ok"] = false, ["description"] = "ответ Telegram не JSON" };
        var ok = reply["ok"] is JsonValue okValue && okValue.TryGetValue<bool>(out var okBool) && okBool;
        long? wait = reply["parameters"] is JsonObject parametersObject
                     && parametersObject["retry_after"] is JsonValue waitValue
                     && waitValue.TryGetValue<long>(out var seconds)
            ? seconds
            : null;
        if (ok || wait is null or 0) return parsed;
        Console.WriteLine($"  Telegram просит подождать {wait} с");
        if (attempt < 2) await Task.Delay(TimeSpan.FromSeconds(wait.Value + 1));
    }
    return last;
}

internal static class PyJson
{
    public static string Serialize(JsonNode? node, int indent = 1)
    {
        var builder = new StringBuilder();
        Write(builder, node, 0, indent);
        return builder.ToString();
    }

    private static void Write(StringBuilder builder, JsonNode? node, int level, int indent)
    {
        switch (node)
        {
            case null:
                builder.Append("null");
                break;
            case JsonObject obj:
                if (obj.Count == 0)
                {
                    builder.Append("{}");
                    break;
                }
                builder.Append('{');
                var first = true;
                foreach (var pair in obj.OrderBy(pair => pair.Key, StringComparer.Ordinal))
                {
                    if (!first) builder.Append(',');
                    first = false;
                    builder.Append('\n').Append(' ', (level + 1) * indent);
                    WriteString(builder, pair.Key);
                    builder.Append(": ");
                    Write(builder, pair.Value, level + 1, indent);
                }
                builder.Append('\n').Append(' ', level * indent).Append('}');
                break;
            case JsonArray array:
                if (array.Count == 0)
                {
                    builder.Append("[]");
                    break;
                }
                builder.Append('[');
                for (var index = 0; index < array.Count; index++)
                {
                    if (index > 0) builder.Append(',');
                    builder.Append('\n').Append(' ', (level + 1) * indent);
                    Write(builder, array[index], level + 1, indent);
                }
                builder.Append('\n').Append(' ', level * indent).Append(']');
                break;
            case JsonValue value:
                if (value.GetValueKind() == JsonValueKind.String)
                    WriteString(builder, value.GetValue<string>());
                else
                    builder.Append(value.ToJsonString());
                break;
        }
    }

    private static void WriteString(StringBuilder builder, string text)
    {
        builder.Append('"');
        foreach (var rune in text.EnumerateRunes())
        {
            switch (rune.Value)
            {
                case '"':
                    builder.Append("\\\"");
                    break;
                case '\\':
                    builder.Append("\\\\");
                    break;
                case '\b':
                    builder.Append("\\b");
                    break;
                case '\f':
                    builder.Append("\\f");
                    break;
                case '\n':
                    builder.Append("\\n");
                    break;
                case '\r':
                    builder.Append("\\r");
                    break;
                case '\t':
                    builder.Append("\\t");
                    break;
                default:
                    if (rune.Value < 0x20)
                        builder.Append("\\u").Append(rune.Value.ToString("x4", CultureInfo.InvariantCulture));
                    else
                        builder.Append(rune.ToString());
                    break;
            }
        }
        builder.Append('"');
    }
}
