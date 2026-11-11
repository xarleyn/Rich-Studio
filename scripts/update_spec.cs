// Bring app/bundled/api.json up to the published Bot API spec.
//
// Downloads the spec (SPEC_URL, by default telegram-bot-api-spec on GitHub), checks its shape,
// and replaces the bundled copy byte for byte when the version or the tracked types changed.
// Prints what changed in the rich types and methods, then what labels.js and render.js still lack
// (the same list the Rust gaps test in src/spec/source.rs checks).
// SPEC_URL may also be a local path or a file:// URL for offline checks.
// Exit code: 0 nothing new, 10 the bundled spec was updated, 1 download or check failed.
//
// Run: dotnet run --file scripts/update_spec.cs

using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;
using System.Net;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;

const string DefaultSpecUrl = "https://raw.githubusercontent.com/PaulSonOfLars/telegram-bot-api-spec/main/api.json";
const int Updated = 10;

Console.OutputEncoding = Encoding.UTF8;

var root = FindRepoRoot();
var bundled = Path.Combine(root, "app", "bundled", "api.json");
var url = Environment.GetEnvironmentVariable("SPEC_URL");
if (string.IsNullOrEmpty(url)) url = DefaultSpecUrl;

byte[] bytes;
try
{
    bytes = await FetchAsync(url);
}
catch (Exception error) when (error is HttpRequestException or IOException or UnauthorizedAccessException
                                  or TaskCanceledException or NotSupportedException or ArgumentException
                                  or UriFormatException)
{
    Console.WriteLine($"не удалось скачать спецификацию: {error.Message}");
    return 1;
}

JsonNode? parsedSpec;
try
{
    parsedSpec = JsonNode.Parse(bytes.AsSpan());
}
catch (JsonException error)
{
    Console.WriteLine($"не удалось скачать спецификацию: {error.Message}");
    return 1;
}

var problem = CheckSpecShape(parsedSpec);
if (problem is not null)
{
    Console.WriteLine($"спецификация отклонена: {problem}");
    return 1;
}
var newSpec = parsedSpec!;

JsonNode oldSpec;
try
{
    oldSpec = JsonNode.Parse(File.ReadAllBytes(bundled)) ?? throw new JsonException("пустой документ");
}
catch (Exception error) when (error is IOException or JsonException or UnauthorizedAccessException)
{
    Console.WriteLine($"не удалось прочитать app/bundled/api.json: {error.Message}");
    return 1;
}

if (CompareVersion(VersionTuple(newSpec["version"]), VersionTuple(oldSpec["version"])) < 0)
{
    Console.WriteLine($"скачана более старая версия {PyText(newSpec["version"])}, в репозитории {PyText(oldSpec["version"])}");
    return 1;
}

var diff = DiffSpecs(oldSpec, newSpec);
if (JsonNode.DeepEquals(newSpec["version"], oldSpec["version"]) && diff.Count == 0)
{
    Console.WriteLine($"{PyText(oldSpec["version"])}: нового нет");
    return 0;
}

try
{
    File.WriteAllBytes(bundled, bytes);
}
catch (Exception error) when (error is IOException or UnauthorizedAccessException)
{
    Console.WriteLine($"не удалось записать app/bundled/api.json: {error.Message}");
    return 1;
}

Console.WriteLine(
    $"{PyText(oldSpec["version"])} → {PyText(newSpec["version"])} ({PyText(newSpec["release_date"])}), {PyText(newSpec["changelog"])}");
foreach (var item in diff) Console.WriteLine($"- {item.Kind}: {item.Name}");

List<Gap> missing;
try
{
    missing = Gaps(root, newSpec["types"] ?? new JsonObject());
}
catch (Exception error) when (error is IOException or InvalidOperationException or UnauthorizedAccessException)
{
    Console.WriteLine($"не удалось проверить подписи и предпросмотр: {error.Message}");
    return 1;
}

Console.WriteLine(
    missing.Count > 0
        ? "\nЧего не хватает редактору (Rust-тест gaps в src/spec/source.rs):"
        : "\nПодписи и предпросмотр уже знают всё новое.");
foreach (var gap in missing)
    Console.WriteLine($"- {gap.Kind} {gap.Name} ({gap.Type}): {gap.Where} в app/static/{gap.File}");
return Updated;

static string FindRepoRoot()
{
    var directory = new DirectoryInfo(Directory.GetCurrentDirectory());
    while (directory is not null)
    {
        if (File.Exists(Path.Combine(directory.FullName, "app", "bundled", "api.json")))
            return directory.FullName;
        directory = directory.Parent;
    }
    throw new InvalidOperationException("не найден корень репозитория (app/bundled/api.json)");
}

static async Task<byte[]> FetchAsync(string source)
{
    if (source.StartsWith("file://", StringComparison.OrdinalIgnoreCase))
        return await File.ReadAllBytesAsync(new Uri(source).LocalPath);
    if (source.StartsWith("http://", StringComparison.OrdinalIgnoreCase)
        || source.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
    {
        using var handler = new HttpClientHandler
        {
            AllowAutoRedirect = true,
            AutomaticDecompression = DecompressionMethods.All,
        };
        using var client = new HttpClient(handler) { Timeout = TimeSpan.FromSeconds(60) };
        using var response = await client.GetAsync(source);
        response.EnsureSuccessStatusCode();
        return await response.Content.ReadAsByteArrayAsync();
    }
    return await File.ReadAllBytesAsync(Path.GetFullPath(source));
}

static string? CheckSpecShape(JsonNode? data)
{
    if (data is not JsonObject spec) return "не объект";
    foreach (var field in new[] { "version", "types", "methods" })
        if (!spec.ContainsKey(field))
            return $"нет поля {field}";
    if (spec["types"] is not JsonObject types || !types.ContainsKey("InputRichBlock")
        || spec["methods"] is not JsonObject methods || !methods.ContainsKey("sendRichMessage"))
        return "нет типов rich-сообщений";
    return null;
}

static string PyText(JsonNode? node) => node switch
{
    null => "None",
    JsonValue value when value.TryGetValue<string>(out var text) => text,
    JsonValue value when value.GetValueKind() == JsonValueKind.True => "True",
    JsonValue value when value.GetValueKind() == JsonValueKind.False => "False",
    _ => node.ToJsonString()
};

static List<long> VersionTuple(JsonNode? version)
{
    var text = version is null ? "" : PyText(version);
    return Regex.Matches(text, @"\d+")
        .Select(match => long.Parse(match.Value, CultureInfo.InvariantCulture))
        .ToList();
}

static int CompareVersion(List<long> left, List<long> right)
{
    for (var index = 0; index < Math.Min(left.Count, right.Count); index++)
    {
        var compared = left[index].CompareTo(right[index]);
        if (compared != 0) return compared;
    }
    return left.Count.CompareTo(right.Count);
}

static bool Tracked(string name) => Track.Prefixes.Any(prefix => name.StartsWith(prefix, StringComparison.Ordinal));

static List<string> FieldNames(JsonNode? definition) =>
    definition?["fields"] is JsonArray fields
        ? fields
            .Select(field => field?["name"] is JsonValue value && value.TryGetValue<string>(out var name) ? name : null)
            .Where(name => name is not null)
            .Select(name => name!)
            .ToList()
        : new List<string>();

static List<string> Subtypes(JsonNode? definition) =>
    definition?["subtypes"] is JsonArray subtypes
        ? subtypes
            .Select(subtype => subtype is JsonValue value && value.TryGetValue<string>(out var name) ? name : null)
            .Where(name => name is not null)
            .Select(name => name!)
            .ToList()
        : new List<string>();

static List<DiffItem> DiffSpecs(JsonNode oldData, JsonNode newData)
{
    var items = new List<DiffItem>();
    var oldTypes = oldData["types"] as JsonObject ?? new JsonObject();
    var newTypes = newData["types"] as JsonObject ?? new JsonObject();
    foreach (var name in newTypes.Select(pair => pair.Key).OrderBy(name => name, StringComparer.Ordinal))
    {
        var newDefinition = newTypes[name];
        if (!oldTypes.ContainsKey(name))
        {
            if (Tracked(name) || name.Contains("Rich", StringComparison.Ordinal))
                items.Add(new DiffItem("type_added", name));
            continue;
        }
        if (!Tracked(name)) continue;
        var oldDefinition = oldTypes[name];
        var oldFields = FieldNames(oldDefinition);
        var newFields = FieldNames(newDefinition);
        foreach (var field in newFields)
            if (!oldFields.Contains(field))
                items.Add(new DiffItem("field_added", $"{name}.{field}"));
        foreach (var field in oldFields)
            if (!newFields.Contains(field))
                items.Add(new DiffItem("field_removed", $"{name}.{field}"));
        var oldSubtypes = Subtypes(oldDefinition).ToHashSet(StringComparer.Ordinal);
        foreach (var subtype in Subtypes(newDefinition).Distinct(StringComparer.Ordinal)
                     .OrderBy(subtype => subtype, StringComparer.Ordinal))
            if (!oldSubtypes.Contains(subtype))
                items.Add(new DiffItem("subtype_added", $"{name} → {subtype}"));
    }
    var oldMethods = oldData["methods"] as JsonObject ?? new JsonObject();
    var newMethods = newData["methods"] as JsonObject ?? new JsonObject();
    foreach (var name in newMethods.Select(pair => pair.Key).OrderBy(name => name, StringComparer.Ordinal))
    {
        if (!oldMethods.ContainsKey(name))
        {
            items.Add(new DiffItem("method_added", name));
            continue;
        }
        if (!Track.Methods.Contains(name)) continue;
        var oldFields = FieldNames(oldMethods[name]);
        foreach (var field in FieldNames(newMethods[name]))
            if (!oldFields.Contains(field))
                items.Add(new DiffItem("param_added", $"{name}({field})"));
    }
    return items;
}

static List<Gap> Gaps(string repoRoot, JsonNode types)
{
    var parts = new[]
    {
        (Part: "labels", File: "js/labels.js", Base: "InputRichBlock", Where: "BLOCK_LABELS"),
        (Part: "labels", File: "js/labels.js", Base: "RichText", Where: "RT_LABELS"),
        (Part: "preview", File: "render.js", Base: "InputRichBlock", Where: "renderBlock"),
        (Part: "preview", File: "render.js", Base: "RichText", Where: "renderRT"),
    };
    var output = new List<Gap>();
    foreach (var part in parts)
    {
        var source = File.ReadAllText(
            Path.Combine(repoRoot, "app", "static", part.File),
            Encoding.UTF8);
        var known = Known(part.Where, source);
        foreach (var (constant, typeName) in Consts(types, part.Base))
            if (!known.Contains(constant))
                output.Add(new Gap(
                    part.Part,
                    part.File,
                    part.Where,
                    part.Base == "InputRichBlock" ? "block" : "inline",
                    constant,
                    typeName));
    }
    return output
        .OrderBy(gap => gap.Part, StringComparer.Ordinal)
        .ThenBy(gap => gap.Kind, StringComparer.Ordinal)
        .ThenBy(gap => gap.Name, StringComparer.Ordinal)
        .ToList();
}

static Dictionary<string, string> Consts(JsonNode types, string baseName)
{
    var output = new Dictionary<string, string>(StringComparer.Ordinal);
    if (types[baseName]?["subtypes"] is not JsonArray subtypes) return output;
    foreach (var subtype in subtypes)
    {
        if (subtype is not JsonValue value || !value.TryGetValue<string>(out var name)) continue;
        var constant = TypeConst(types, name);
        if (constant is { } found) output[found.Value] = name;
    }
    return output;
}

static (string Field, string Value)? TypeConst(JsonNode types, string typeName)
{
    if (types[typeName]?["fields"] is not JsonArray fields) return null;
    foreach (var field in fields)
    {
        if (field?["name"] is not JsonValue nameValue || !nameValue.TryGetValue<string>(out var name)) continue;
        if (name is not ("type" or "source" or "status")) continue;
        var description = field["description"] is JsonValue descriptionValue
                          && descriptionValue.TryGetValue<string>(out var text)
            ? text
            : "";
        var constant = Regex.Match(description, "(?:always\\s+[“\"]([^”\"]+)[”\"]|must be ([a-z_]+)$)");
        if (constant.Success)
            return (name, constant.Groups[1].Success ? constant.Groups[1].Value : constant.Groups[2].Value);
    }
    return null;
}

static HashSet<string> Known(string where, string source) => where switch
{
    "renderBlock" => FunctionCases(source, "renderBlock", inline: false),
    "renderRT" => Union(FunctionCases(source, "renderRT", inline: true), ObjectKeys(source, "WRAP")),
    _ => ObjectKeys(source, where),
};

static HashSet<string> Union(HashSet<string> left, HashSet<string> right)
{
    left.UnionWith(right);
    return left;
}

static HashSet<string> ObjectKeys(string source, string variable)
{
    var found = Regex.Match(
        source,
        $@"\b{Regex.Escape(variable)}\s*=\s*\{{(.*?)\}};",
        RegexOptions.Singleline);
    if (!found.Success) throw new InvalidOperationException($"{variable} not found");
    return Regex.Matches(found.Groups[1].Value, @"(?:^|[,{\s])([a-z_]+)\s*:")
        .Select(match => match.Groups[1].Value)
        .ToHashSet(StringComparer.Ordinal);
}

static HashSet<string> FunctionCases(string source, string function, bool inline)
{
    var start = source.IndexOf($"function {function}(", StringComparison.Ordinal);
    if (start < 0) throw new InvalidOperationException($"{function} not found");
    var end = source.IndexOf("\n  function ", start + 1, StringComparison.Ordinal);
    var body = end < 0 ? source[start..] : source[start..end];
    return Regex.Matches(body, inline ? "'([a-z_]+)'" : "case '([a-z_]+)'")
        .Select(match => match.Groups[1].Value)
        .ToHashSet(StringComparer.Ordinal);
}

internal sealed record DiffItem(string Kind, string Name);

internal sealed record Gap(string Part, string File, string Where, string Kind, string Name, string Type);

internal static class Track
{
    public static readonly string[] Prefixes =
    {
        "InputRich",
        "RichText",
        "RichBlock",
        "RichMessage",
        "InlineKeyboard",
        "CopyTextButton",
        "DisabledButton",
        "InputMedia",
        "LinkPreview",
    };

    public static readonly string[] Methods =
    {
        "sendRichMessage",
        "editMessageText",
        "sendMessage",
        "editMessageReplyMarkup",
        "pinChatMessage",
        "deleteMessage",
        "getChatMember",
        "getMe",
    };
}
