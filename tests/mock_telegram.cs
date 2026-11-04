#:sdk Microsoft.NET.Sdk.Web
#:property TargetFramework=net10.0
#:property PublishAot=false

// Dev-only fake Bot API for Playwright browser scenarios (tests/e2e_ui.mjs).
// Rust tests do not use it: they use in-process mocks, and the optional
// external tgfake suite is described in docs/TGFAKE.md.
//
// Run from the repository root, then start the studio against it:
//   dotnet run --file tests/mock_telegram.cs -- --urls http://127.0.0.1:8081
//   TG_BOT_TOKEN=1:x TG_API_BASE=http://127.0.0.1:8081 ...
// GET /log shows every call; POST /push queues an update that the studio will
// receive through getUpdates; POST /fail makes a method answer 400 (or the
// given error_code) for checks of refused sends.
//
// The .NET 10 file-based Web SDK defaults to the AOT profile, where
// System.Text.Json cannot use reflection; this dev stand has no such goal, so
// PublishAot=false keeps Results.Json simple and the build warning-free.
// TargetFramework pins the app to .NET 10 when a newer SDK is installed.

using System.Globalization;
using System.Text.Json.Nodes;

var capturedPath = FindCaptured();
if (capturedPath is null)
{
    Console.Error.WriteLine("mock telegram: не найден tests/telegram_parse/captured.json");
    Console.Error.WriteLine("запускайте из корня репозитория: dotnet run --file tests/mock_telegram.cs -- --urls http://127.0.0.1:8081");
    return 1;
}

var mock = new FakeTelegram(capturedPath);
var builder = WebApplication.CreateBuilder(args);
var app = builder.Build();

app.MapPost("/bot{token}/{method}", async Task<IResult> (HttpContext context, string method) =>
{
    var (body, files) = await ReadBody(context.Request);
    // getUpdates is a poll, not a Bot API call the tests look for in GET /log.
    if (method == "getUpdates") return Results.Json(await mock.GetUpdates(body));
    return Results.Json(mock.RecordAndAnswer(method, body, files));
});
app.MapPost("/push", async Task<IResult> (HttpContext context) =>
    Results.Json(mock.Push(await ReadJsonObject(context.Request))));
app.MapPost("/fail", async Task<IResult> (HttpContext context) =>
{
    mock.SetFail(await ReadJsonObject(context.Request));
    return Results.Json(new JsonObject { ["ok"] = true });
});
app.MapGet("/log", () => Results.Json(mock.Log()));

app.Run();
return 0;

static string? FindCaptured()
{
    // `dotnet run --file` starts the app with the script's directory as CWD, so look
    // for tests/telegram_parse/captured.json from CWD and then up its parents.
    for (string? directory = Directory.GetCurrentDirectory(); directory is not null; directory = Path.GetDirectoryName(directory))
    {
        foreach (var candidate in new[]
                 {
                     Path.Combine(directory, "tests", "telegram_parse", "captured.json"),
                     Path.Combine(directory, "telegram_parse", "captured.json"),
                 })
        {
            if (File.Exists(candidate)) return candidate;
        }
    }
    return null;
}

static async Task<(JsonObject Body, Dictionary<string, JsonArray> Files)> ReadBody(HttpRequest request)
{
    var files = new Dictionary<string, JsonArray>(StringComparer.Ordinal);
    if (request.ContentType?.Contains("multipart", StringComparison.OrdinalIgnoreCase) == true)
    {
        var form = await request.ReadFormAsync();
        var body = new JsonObject();
        foreach (var field in form)
            body[field.Key] = field.Value.Count > 0 ? JsonValue.Create(field.Value[field.Value.Count - 1]) : JsonValue.Create(string.Empty);
        foreach (var file in form.Files)
            files[file.Name] = new JsonArray(JsonValue.Create(file.FileName), JsonValue.Create(file.Length));
        // multipart sends rich_message and reply_markup as JSON strings, not objects
        foreach (var key in new[] { "rich_message", "reply_markup" })
            if (body[key] is JsonValue value && value.TryGetValue<string>(out var text))
                body[key] = JsonNode.Parse(text);
        return (body, files);
    }
    return (await ReadJsonObject(request), files);
}

static async Task<JsonObject> ReadJsonObject(HttpRequest request)
{
    var node = await JsonNode.ParseAsync(request.Body);
    return node as JsonObject ?? throw new BadHttpRequestException("request body must be a JSON object");
}

sealed record Call(string Method, JsonObject Body, Dictionary<string, JsonArray> Files);

/// <summary>Records every Bot API call and answers like Telegram would.</summary>
sealed class FakeTelegram
{
    private static readonly HashSet<string> TrueMethods = new(StringComparer.Ordinal)
    {
        "deleteMessage",
        "pinChatMessage",
        "unpinChatMessage",
        "answerCallbackQuery",
        "approveSuggestedPost",
        "declineSuggestedPost",
    };

    private static readonly HashSet<string> InviteMethods = new(StringComparer.Ordinal)
    {
        "createChatInviteLink",
        "editChatInviteLink",
        "revokeChatInviteLink",
        "createChatSubscriptionInviteLink",
        "editChatSubscriptionInviteLink",
    };

    private static readonly string[] InviteKeptFields =
    [
        "name",
        "expire_date",
        "member_limit",
        "creates_join_request",
        "subscription_period",
        "subscription_price",
    ];

    private const string NotModifiedDescription =
        "Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message";

    private readonly object gate = new();
    private readonly List<Call> calls = [];
    private readonly List<JsonObject> updates = [];
    private readonly Dictionary<string, JsonNode> fail = new(StringComparer.Ordinal);
    private readonly Dictionary<string, JsonNode> failCode = new(StringComparer.Ordinal);
    private readonly Dictionary<string, JsonObject> messages = new(StringComparer.Ordinal); // chat_id + message_id -> content
    private readonly Dictionary<string, JsonObject> links = new(StringComparer.Ordinal); // invite_link -> ChatInviteLink
    private readonly Dictionary<(string Format, string Source), JsonNode> parsedCache = [];
    private long nextId = 100;
    private long nextUpdate = 1;

    public FakeTelegram(string capturedPath)
    {
        // real answers of Telegram to known HTML/Markdown sources (scripts/capture_telegram_parse.py)
        var captured = JsonNode.Parse(File.ReadAllText(capturedPath)) as JsonObject
            ?? throw new InvalidOperationException($"{capturedPath}: ожидался JSON-объект с ответами Telegram");
        foreach (var entry in captured)
        {
            if (entry.Value is not JsonObject item) continue;
            if (item["blocks"] is not JsonArray blocks || blocks.Count == 0) continue;
            if (item["format"] is not JsonValue formatValue || !formatValue.TryGetValue<string>(out var format)) continue;
            if (item["source"] is not JsonValue sourceValue || !sourceValue.TryGetValue<string>(out var source)) continue;
            parsedCache[(format, source)] = blocks.DeepClone();
        }
    }

    public JsonObject RecordAndAnswer(string method, JsonObject body, Dictionary<string, JsonArray> files)
    {
        lock (gate)
        {
            calls.Add(new Call(method, body, files));
            return AnswerLocked(method, body);
        }
    }

    public JsonArray Log()
    {
        lock (gate)
        {
            var result = new JsonArray();
            foreach (var call in calls)
                result.Add(new JsonObject { ["method"] = call.Method, ["body"] = call.Body.DeepClone() });
            return result;
        }
    }

    public JsonObject Push(JsonObject update)
    {
        lock (gate)
        {
            update["update_id"] = nextUpdate;
            nextUpdate += 1;
            updates.Add((JsonObject)update.DeepClone());
            return update;
        }
    }

    public async Task<JsonObject> GetUpdates(JsonObject body)
    {
        // short long-poll: min(timeout, 2) seconds in 100 ms steps, like the Python mock
        var iterations = (int)(Math.Min(Number(body["timeout"]), 2.0) * 10);
        for (var i = 0; i < iterations; i++)
        {
            lock (gate)
            {
                if (updates.Count > 0) break;
            }
            await Task.Delay(TimeSpan.FromMilliseconds(100));
        }
        var offset = Integer(body["offset"]);
        lock (gate)
        {
            var result = new JsonArray();
            foreach (var update in updates)
                if (Integer(update["update_id"]) >= offset) result.Add(update.DeepClone());
            updates.Clear();
            return new JsonObject { ["ok"] = true, ["result"] = result };
        }
    }

    public void SetFail(JsonObject request)
    {
        var method = RequiredString(request, "method");
        lock (gate)
        {
            var description = request["description"];
            if (!Truthy(description))
            {
                fail.Remove(method);
                failCode.Remove(method);
                return;
            }
            fail[method] = description!.DeepClone();
            var code = request["error_code"];
            failCode[method] = Truthy(code) ? code!.DeepClone() : JsonValue.Create(400);
        }
    }

    private JsonObject AnswerLocked(string method, JsonObject body)
    {
        if (fail.TryGetValue(method, out var failure))
            return Error(failCode.TryGetValue(method, out var code) ? code.DeepClone() : 400, failure.DeepClone());

        switch (method)
        {
            case "getMe":
                return Ok(new JsonObject
                {
                    ["id"] = 42,
                    ["is_bot"] = true,
                    ["first_name"] = "Bot",
                    ["username"] = "studio_test_bot",
                });
            case "setMyCommands":
                return Ok(true);
            case "sendMessage":
                nextId += 1;
                return Ok(new JsonObject
                {
                    ["message_id"] = nextId,
                    ["chat"] = new JsonObject { ["id"] = Required(body, "chat_id").DeepClone(), ["type"] = "private" },
                    ["text"] = Required(body, "text").DeepClone(),
                });
            case "getChat":
                return Ok(new JsonObject { ["id"] = -1001, ["type"] = "channel", ["title"] = "Тест" });
            case "getChatMember":
                return Ok(new JsonObject
                {
                    ["status"] = "administrator",
                    ["can_post_messages"] = true,
                    ["can_edit_messages"] = true,
                });
            case "sendLivePhoto":
            case "sendPaidMedia":
            {
                nextId += 1;
                var message = Message(nextId, ("caption", body["caption"]));
                if (method == "sendLivePhoto")
                {
                    message["live_photo"] = new JsonObject
                    {
                        ["file_id"] = "lp",
                        ["file_unique_id"] = "lp",
                        ["width"] = 720,
                        ["height"] = 960,
                        ["duration"] = 2,
                    };
                }
                else
                {
                    message["paid_media"] = new JsonObject
                    {
                        ["star_count"] = Integer(body["star_count"]),
                        ["paid_media"] = new JsonArray(),
                    };
                }
                return Ok(message);
            }
            case "sendRichMessage":
            case "sendPoll":
            {
                nextId += 1;
                var message = Message(nextId);
                messages[MessageKey(body["chat_id"], nextId)] = Content(body);
                if (method == "sendRichMessage")
                    message["rich_message"] = new JsonObject { ["blocks"] = ParsedBlocks(body["rich_message"]) };
                else
                    message["poll"] = new JsonObject { ["id"] = $"poll{nextId}", ["question"] = body["question"]?.DeepClone() };
                return Ok(message);
            }
            case "editMessageText":
            {
                // like Telegram: the edited Message for channel posts, 400 when nothing would change
                var key = MessageKey(body["chat_id"], body["message_id"]);
                var content = Content(body);
                if (messages.TryGetValue(key, out var stored) && JsonNode.DeepEquals(stored, content))
                    return NotModified();
                messages[key] = content;
                return Ok(Message(body["message_id"], ("edit_date", JsonValue.Create(UnixTime()))));
            }
            case "editMessageReplyMarkup":
            {
                // the same "not modified" as editMessageText when the buttons are the same
                var key = MessageKey(body["chat_id"], body["message_id"]);
                messages.TryGetValue(key, out var stored);
                if (stored is not null && JsonNode.DeepEquals(stored["reply_markup"], body["reply_markup"]))
                    return NotModified();
                if (stored is not null) stored["reply_markup"] = body["reply_markup"]?.DeepClone();
                return Ok(Message(body["message_id"], ("edit_date", JsonValue.Create(UnixTime()))));
            }
            case "stopPoll":
                return Ok(new JsonObject
                {
                    ["id"] = "x",
                    ["total_voter_count"] = 3,
                    ["is_closed"] = true,
                    ["options"] = new JsonArray(
                        new JsonObject { ["text"] = "Да", ["voter_count"] = 2 },
                        new JsonObject { ["text"] = "Нет", ["voter_count"] = 1 }),
                });
            default:
                if (InviteMethods.Contains(method)) return Ok(InviteLink(method, body));
                if (TrueMethods.Contains(method)) return Ok(true);
                return Error(404, "Not Found: method not mocked");
        }
    }

    private JsonObject InviteLink(string method, JsonObject body)
    {
        // a ChatInviteLink as Telegram answers: the link's own fields back, a new t.me/+ link on create
        string link;
        if (method.StartsWith("create", StringComparison.Ordinal))
        {
            nextId += 1;
            link = $"https://t.me/+fake{nextId}";
        }
        else
        {
            link = RequiredString(body, "invite_link");
        }
        var previous = links.TryGetValue(link, out var existing) ? existing : new JsonObject();
        if (method == "revokeChatInviteLink")
        {
            var revoked = (JsonObject)previous.DeepClone();
            revoked["is_revoked"] = true;
            links[link] = revoked;
            return revoked;
        }
        var result = new JsonObject
        {
            ["invite_link"] = link,
            ["creator"] = new JsonObject { ["id"] = 42, ["is_bot"] = true, ["first_name"] = "Bot" },
            ["creates_join_request"] = false,
            ["is_primary"] = false,
            ["is_revoked"] = false,
        };
        foreach (var field in new[] { "subscription_period", "subscription_price" })
            if (previous.TryGetPropertyValue(field, out var value))
                result[field] = value?.DeepClone();
        foreach (var field in InviteKeptFields)
            if (body.TryGetPropertyValue(field, out var value) && !IsNullOrEmptyString(value))
                result[field] = value?.DeepClone();
        links[link] = result;
        return result;
    }

    private JsonNode? ParsedBlocks(JsonNode? richMessage)
    {
        // the blocks Telegram answers with: recorded ones for a known source, else the text as one paragraph
        if (richMessage is JsonValue value && value.TryGetValue<string>(out var serialized))
            richMessage = JsonNode.Parse(serialized);
        if (richMessage is not JsonObject message) return new JsonArray();
        if (message.TryGetPropertyValue("blocks", out var blocks)) return blocks?.DeepClone();
        foreach (var format in new[] { "html", "markdown" })
        {
            if (!message.TryGetPropertyValue(format, out var source)) continue;
            if (source is JsonValue sourceValue && sourceValue.TryGetValue<string>(out var text)
                && parsedCache.TryGetValue((format, text), out var cached))
                return cached.DeepClone();
            return new JsonArray(new JsonObject { ["type"] = "paragraph", ["text"] = source?.DeepClone() });
        }
        return new JsonArray();
    }

    private static JsonObject Content(JsonObject body) => new()
    {
        ["rich_message"] = body["rich_message"]?.DeepClone(),
        ["reply_markup"] = body["reply_markup"]?.DeepClone(),
    };

    private static JsonObject Message(JsonNode? messageId, params (string Name, JsonNode? Value)[] extra)
    {
        // a Message the way real Telegram returns it (not the full rich_message, which Telegram rewrites into blocks)
        var message = new JsonObject
        {
            ["message_id"] = messageId?.DeepClone(),
            ["from"] = new JsonObject { ["id"] = 42, ["is_bot"] = true, ["first_name"] = "Bot", ["username"] = "studio_test_bot" },
            ["chat"] = new JsonObject { ["id"] = -1001, ["type"] = "channel", ["title"] = "Тест" },
            ["date"] = UnixTime(),
        };
        foreach (var (name, value) in extra) message[name] = value?.DeepClone();
        return message;
    }

    private static string MessageKey(JsonNode? chatId, JsonNode? messageId) => $"{KeyPart(chatId)}\u0000{KeyPart(messageId)}";

    private static string KeyPart(JsonNode? node) => node is null ? "\u0001null" : node.ToJsonString();

    private static JsonObject Ok(JsonNode? result) => new() { ["ok"] = true, ["result"] = result };

    private static JsonObject Error(JsonNode? code, JsonNode? description) => new()
    {
        ["ok"] = false,
        ["error_code"] = code,
        ["description"] = description,
    };

    private static JsonObject NotModified() => Error(400, NotModifiedDescription);

    private static JsonNode Required(JsonObject body, string field) =>
        body[field] ?? throw new InvalidOperationException($"в запросе нет обязательного поля {field}");

    private static string RequiredString(JsonObject body, string field) => Required(body, field).GetValue<string>();

    private static bool IsNullOrEmptyString(JsonNode? node) =>
        node is null || (node is JsonValue value && value.TryGetValue<string>(out var text) && text.Length == 0);

    private static long UnixTime() => DateTimeOffset.UtcNow.ToUnixTimeSeconds();

    private static double Number(JsonNode? node)
    {
        if (node is JsonValue value)
        {
            if (value.TryGetValue<double>(out var real)) return real;
            if (value.TryGetValue<long>(out var integer)) return integer;
            if (value.TryGetValue<string>(out var text) && double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out var parsed))
                return parsed;
        }
        return 0;
    }

    private static long Integer(JsonNode? node)
    {
        if (!Truthy(node)) return 0;
        if (node is JsonValue value)
        {
            if (value.TryGetValue<long>(out var integer)) return integer;
            if (value.TryGetValue<double>(out var real)) return (long)real;
            if (value.TryGetValue<bool>(out var flag)) return flag ? 1 : 0;
            if (value.TryGetValue<string>(out var text)
                && long.TryParse(text.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsed))
                return parsed;
        }
        throw new InvalidOperationException($"не целое число: {node!.ToJsonString()}");
    }

    private static bool Truthy(JsonNode? node)
    {
        // Python truthiness of a JSON value, which /fail and getUpdates use
        if (node is null) return false;
        if (node is JsonValue value)
        {
            if (value.TryGetValue<bool>(out var flag)) return flag;
            if (value.TryGetValue<string>(out var text)) return text.Length > 0;
            if (value.TryGetValue<double>(out var number)) return number != 0;
            return true;
        }
        if (node is JsonArray array) return array.Count > 0;
        if (node is JsonObject obj) return obj.Count > 0;
        return true;
    }
}
