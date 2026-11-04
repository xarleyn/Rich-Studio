#!/usr/bin/env -S dotnet run --file
#:package SkiaSharp@4.154.0
#:package SkiaSharp.NativeAssets.Win32@4.154.0
#:package SkiaSharp.NativeAssets.Linux.NoDependencies@4.154.0
#:package SkiaSharp.NativeAssets.macOS@4.154.0

// Rebuild the public derivatives checked in under app/static: WebP screenshots,
// the 120 px icon, favicon.ico, the social preview and precompressed Brotli
// copies of landing.css / landing.js. The manifest in app/bundled/public-assets.json
// records every source and output hash.
//
// Usage (repository root, .NET 10 SDK):
//   dotnet run --file scripts/optimize-public-assets.cs             # rewrite
//   dotnet run --file scripts/optimize-public-assets.cs -- --check  # compare only
//   dotnet run --file scripts/optimize-public-assets.cs -- --root D:/repos/Rich-Studio
//
// The Manrope WOFF2 is a fontTools pyftsubset artefact; SkiaSharp cannot subset
// a TTF into WOFF2, so this script keeps the checked-in file and only verifies
// that manrope.ttf still matches the manifest.

using System.IO.Compression;
using System.Security.Cryptography;
using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using SkiaSharp;

const int WebpQuality = 86;
const int SocialWidth = 1200;
const int SocialHeight = 630;
const int ScreenshotWidth = 1120;
const int ScreenshotHeight = 530;
const string BrandText = "RichPost";

var check = false;
string? rootArgument = null;
for (var index = 0; index < args.Length; index++)
{
    switch (args[index])
    {
        case "--check":
            check = true;
            break;
        case "--root":
            if (index + 1 >= args.Length)
            {
                Console.Error.WriteLine("error: --root needs a directory");
                return 2;
            }
            rootArgument = args[++index];
            break;
        case "-h":
        case "--help":
            Console.WriteLine(
                """
                Rebuild public derivatives for Rich Studio.

                  (no flags)  regenerate WebP, icon-120, favicon.ico, social-preview.png,
                              landing.css.br, landing.js.br and app/bundled/public-assets.json
                  --check     recompute everything in memory and compare with the checked-in
                              files and manifest; exits 1 on any difference, writes nothing
                  --root DIR  repository root (default: current directory)
                """);
            return 0;
        default:
            Console.Error.WriteLine($"error: unknown argument: {args[index]}");
            return 2;
    }
}

var root = Path.GetFullPath(rootArgument ?? Directory.GetCurrentDirectory());
var staticDirectory = Path.Combine(root, "app", "static");
var manifestPath = Path.Combine(root, "app", "bundled", "public-assets.json");
var fontPath = Path.Combine(staticDirectory, "fonts", "manrope.ttf");
var woff2Path = Path.Combine(staticDirectory, "fonts", "manrope.woff2");

if (!Directory.Exists(staticDirectory) || !File.Exists(manifestPath) || !File.Exists(fontPath))
{
    Console.Error.WriteLine(
        $"error: {root} is not a Rich Studio repository root (app/static, app/bundled/public-assets.json and app/static/fonts/manrope.ttf are required)");
    return 2;
}

JsonObject manifest;
try
{
    manifest = JsonNode.Parse(File.ReadAllText(manifestPath))?.AsObject()
        ?? throw new InvalidDataException("manifest is not a JSON object");
}
catch (Exception exception) when (exception is JsonException or InvalidDataException)
{
    Console.Error.WriteLine($"error: cannot read {manifestPath}: {exception.Message}");
    return 2;
}

var fontSourceSha = HashFile(fontPath);
var recordedFontSource = ManifestString(manifest, "font", "source_sha256");
if (!string.Equals(fontSourceSha, recordedFontSource, StringComparison.OrdinalIgnoreCase))
{
    Console.Error.WriteLine(
        $"""
        error: app/static/fonts/manrope.ttf changed (manifest source_sha256 {recordedFontSource}, file {fontSourceSha}).

        Manrope WOFF2 is a fontTools pyftsubset artefact; SkiaSharp cannot subset a TTF
        into WOFF2. Rebuild it once, then rerun this script so it records the new hash:

          python -m venv work/seo-tools
          work/seo-tools/Scripts/python -m pip install 'fonttools[woff]'
          work/seo-tools/Scripts/pyftsubset app/static/fonts/manrope.ttf --output-file=app/static/fonts/manrope.woff2 --flavor=woff2 --unicodes=U+0020-0250,U+0400-0530,U+1E00-1F00,U+2000-2300 --no-recalc-timestamp
        """);
    return 2;
}
if (!File.Exists(woff2Path))
{
    Console.Error.WriteLine($"error: {woff2Path} is missing; rebuild it with pyftsubset as described in the README");
    return 2;
}
var woff2Sha = HashFile(woff2Path);
var recordedWoff2 = ManifestString(manifest, "font", "sha256");

var failures = new List<string>();
var matches = new List<string>();
var icon512Path = Path.Combine(staticDirectory, "icons", "icon-512.png");
var socialPreviewPath = Path.Combine(staticDirectory, "images", "social-preview.png");
var screenshotPath = Path.Combine(staticDirectory, "images", "landing-editor.jpg");

if (!File.Exists(icon512Path) || !File.Exists(screenshotPath))
{
    Console.Error.WriteLine("error: app/static/icons/icon-512.png and app/static/images/landing-editor.jpg are required");
    return 2;
}

// ---- derived WebP screenshots -------------------------------------------------

var derived = new JsonObject();
foreach (var name in new[] { "landing-editor", "landing-preview", "landing-schedule" })
{
    var sourceRelative = $"images/{name}.jpg";
    var outputRelative = $"images/{name}.webp";
    var sourcePath = Path.Combine(staticDirectory, "images", $"{name}.jpg");
    var outputPath = Path.Combine(staticDirectory, "images", $"{name}.webp");
    if (!File.Exists(sourcePath))
    {
        Console.Error.WriteLine($"error: {sourceRelative} is missing");
        return 2;
    }
    var sourceSha = HashFile(sourcePath);
    var webp = EncodeWebp(sourcePath, WebpQuality);
    derived[outputRelative] = new JsonObject
    {
        ["source"] = sourceRelative,
        ["source_sha256"] = sourceSha,
        ["sha256"] = Hash(webp),
    };

    var itemFailures = failures.Count;
    var recorded = manifest["derived"]?[outputRelative] as JsonObject;
    if (recorded is null)
    {
        failures.Add($"{outputRelative}: absent from the manifest");
    }
    else
    {
        if (!SameHash(ManifestString(recorded, "source_sha256"), sourceSha))
            failures.Add($"{outputRelative}: source {sourceRelative} hash {sourceSha} differs from manifest {ManifestString(recorded, "source_sha256")}");
        if (!SameHash(ManifestString(recorded, "sha256"), Hash(webp)))
            failures.Add($"{outputRelative}: regenerated WebP hash {Hash(webp)} differs from manifest {ManifestString(recorded, "sha256")}");
    }
    if (!File.Exists(outputPath))
    {
        failures.Add($"{outputRelative}: file is missing");
    }
    else
    {
        var onDisk = HashFile(outputPath);
        if (!SameHash(onDisk, Hash(webp)))
            failures.Add($"{outputRelative}: regenerated WebP ({webp.Length} bytes, {Hash(webp)}) differs from checked-in file ({new FileInfo(outputPath).Length} bytes, {onDisk})");
        if (recorded is not null && !SameHash(ManifestString(recorded, "sha256"), onDisk))
            failures.Add($"{outputRelative}: checked-in file hash {onDisk} differs from manifest {ManifestString(recorded, "sha256")}");
    }
    if (failures.Count == itemFailures)
        matches.Add($"{outputRelative}: {webp.Length} bytes, sha256 {Hash(webp)}");

    if (!check)
        File.WriteAllBytes(outputPath, webp);
}

// ---- icons --------------------------------------------------------------------

using var icon = Decode(icon512Path);
byte[] icon120;
using (var resized = Resize(icon, 120, 120))
    icon120 = EncodePng(resized, "icons/icon-120.png");
var favicon = BuildIco(icon, new[] { 16, 32, 48, 64 });

if (CompareOrWrite("icons/icon-120.png", Path.Combine(staticDirectory, "icons", "icon-120.png"), icon120))
    matches.Add($"icons/icon-120.png: {icon120.Length} bytes, sha256 {Hash(icon120)}");
if (CompareOrWrite("icons/favicon.ico", Path.Combine(staticDirectory, "icons", "favicon.ico"), favicon))
    matches.Add($"icons/favicon.ico: {favicon.Length} bytes, sha256 {Hash(favicon)}");

// ---- social preview -----------------------------------------------------------

var social = BuildSocialPreview(screenshotPath, fontPath);
if (CompareOrWrite("images/social-preview.png", socialPreviewPath, social))
    matches.Add($"images/social-preview.png: {social.Length} bytes, sha256 {Hash(social)}");

// ---- precompressed CSS/JS -----------------------------------------------------

var compressed = new JsonObject();
foreach (var name in new[] { "landing.css", "landing.js" })
{
    var sourcePath = Path.Combine(staticDirectory, name);
    var outputPath = Path.Combine(staticDirectory, name + ".br");
    if (!File.Exists(sourcePath))
    {
        Console.Error.WriteLine($"error: {name} is missing");
        return 2;
    }
    var sourceSha = HashFile(sourcePath);
    var brotli = BrotliCompress(File.ReadAllBytes(sourcePath));
    compressed[name] = new JsonObject
    {
        ["source_sha256"] = sourceSha,
        ["brotli_sha256"] = Hash(brotli),
    };

    var itemFailures = failures.Count;
    var recorded = manifest["compressed"]?[name] as JsonObject;
    if (recorded is null)
    {
        failures.Add($"{name}: absent from the manifest");
    }
    else
    {
        if (!SameHash(ManifestString(recorded, "source_sha256"), sourceSha))
            failures.Add($"{name}: source hash {sourceSha} differs from manifest {ManifestString(recorded, "source_sha256")}");
        if (!SameHash(ManifestString(recorded, "brotli_sha256"), Hash(brotli)))
            failures.Add($"{name}.br: regenerated Brotli hash {Hash(brotli)} differs from manifest {ManifestString(recorded, "brotli_sha256")}");
    }
    if (!File.Exists(outputPath))
    {
        failures.Add($"{name}.br: file is missing");
    }
    else
    {
        var onDisk = HashFile(outputPath);
        if (!SameHash(onDisk, Hash(brotli)))
            failures.Add($"{name}.br: regenerated Brotli ({brotli.Length} bytes, {Hash(brotli)}) differs from checked-in file ({new FileInfo(outputPath).Length} bytes, {onDisk})");
        if (recorded is not null && !SameHash(ManifestString(recorded, "brotli_sha256"), onDisk))
            failures.Add($"{name}.br: checked-in file hash {onDisk} differs from manifest {ManifestString(recorded, "brotli_sha256")}");
    }
    if (failures.Count == itemFailures)
        matches.Add($"{name}.br: {brotli.Length} bytes, sha256 {Hash(brotli)}");

    if (!check)
        File.WriteAllBytes(outputPath, brotli);
}

// ---- font manifest entry ------------------------------------------------------

if (!SameHash(recordedWoff2, woff2Sha))
{
    failures.Add($"fonts/manrope.woff2: hash {woff2Sha} differs from manifest {recordedWoff2}");
}
else
{
    matches.Add($"fonts/manrope.woff2: {woff2Sha} (fontTools artefact, left untouched)");
}

// ---- manifest -----------------------------------------------------------------

var rebuilt = new JsonObject
{
    ["font"] = new JsonObject
    {
        ["source_sha256"] = fontSourceSha,
        ["sha256"] = woff2Sha,
    },
    ["derived"] = derived,
    ["compressed"] = compressed,
};
var encoded = rebuilt.ToJsonString(
    new JsonSerializerOptions
    {
        WriteIndented = true,
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    }).ReplaceLineEndings("\n") + "\n";

if (check)
{
    var current = File.ReadAllText(manifestPath).ReplaceLineEndings("\n");
    if (current != encoded)
        failures.Add("app/bundled/public-assets.json: would be rewritten (rerun without --check)");
    else
        matches.Add("app/bundled/public-assets.json: up to date");
    foreach (var match in matches)
        Console.WriteLine($"match: {match}");
    if (failures.Count == 0)
    {
        Console.WriteLine("ok: public assets and app/bundled/public-assets.json match");
        return 0;
    }
    foreach (var failure in failures)
        Console.Error.WriteLine($"diff: {failure}");
    Console.Error.WriteLine($"{failures.Count} difference(s); nothing was written");
    return 1;
}

foreach (var failure in failures)
    Console.Error.WriteLine($"warning: {failure} (rewritten)");
File.WriteAllText(manifestPath, encoded, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false));

string[] written =
[
    woff2Path,
    .. new[] { "landing-editor", "landing-preview", "landing-schedule" }
        .Select(name => Path.Combine(staticDirectory, "images", name + ".webp")),
    socialPreviewPath,
    Path.Combine(staticDirectory, "icons", "icon-120.png"),
    Path.Combine(staticDirectory, "icons", "favicon.ico"),
    Path.Combine(staticDirectory, "landing.css.br"),
    Path.Combine(staticDirectory, "landing.js.br"),
];
foreach (var path in written)
{
    Console.WriteLine($"{Path.GetRelativePath(staticDirectory, path).Replace('\\', '/')}: {new FileInfo(path).Length} bytes");
}
return 0;

// ---- helpers ------------------------------------------------------------------

bool CompareOrWrite(string relative, string path, byte[] generated)
{
    var matched = false;
    if (!File.Exists(path))
    {
        failures.Add($"{relative}: file is missing");
    }
    else
    {
        var onDisk = HashFile(path);
        matched = SameHash(onDisk, Hash(generated));
        if (!matched)
            failures.Add($"{relative}: regenerated file ({generated.Length} bytes, {Hash(generated)}) differs from checked-in file ({new FileInfo(path).Length} bytes, {onDisk})");
    }
    if (!check)
        File.WriteAllBytes(path, generated);
    return matched;
}

static string HashFile(string path) => Hash(File.ReadAllBytes(path));

static string Hash(byte[] bytes) => Convert.ToHexStringLower(SHA256.HashData(bytes));

static bool SameHash(string expected, string actual) =>
    string.Equals(expected, actual, StringComparison.OrdinalIgnoreCase);

static string ManifestString(JsonObject parent, params string[] path)
{
    JsonNode? node = parent;
    foreach (var part in path)
        node = node?[part];
    return node?.GetValue<string>()
        ?? throw new InvalidDataException($"manifest lacks {string.Join('.', path)}");
}

static SKBitmap Decode(string path) =>
    SKBitmap.Decode(path) ?? throw new InvalidDataException($"cannot decode {path}");

// SkiaSharp exposes no Lanczos filter; Mitchell cubic is the closest
// high-quality resampler to the LANCZOS kernel Pillow used.
static SKBitmap Resize(SKBitmap source, int width, int height) =>
    source.Resize(new SKImageInfo(width, height, source.ColorType, source.AlphaType), new SKSamplingOptions(SKCubicResampler.Mitchell))
    ?? throw new InvalidOperationException($"cannot resize to {width}x{height}");

static byte[] EncodeWebp(string path, int quality)
{
    using var bitmap = Decode(path);
    using var image = SKImage.FromBitmap(bitmap);
    // Skia delegates to libwebp with its default effort; Pillow used method=6, so
    // the bytes differ even though both encode at quality 86.
    using var data = image.Encode(SKEncodedImageFormat.Webp, quality)
        ?? throw new InvalidOperationException($"cannot encode {path} as WebP");
    return data.ToArray();
}

static byte[] EncodePng(SKBitmap bitmap, string label)
{
    using var stream = new MemoryStream();
    // Match Pillow's `optimize=True`: evaluate every PNG row filter and use the
    // maximum zlib level. The bytes still differ from Pillow's encoder, but the
    // size stays in the same range.
    var pixmap = bitmap.PeekPixels()
        ?? throw new InvalidOperationException($"cannot read pixels of {label}");
    pixmap.Encode(stream, new SKPngEncoderOptions(SKPngEncoderFilterFlags.AllFilters, 9));
    return stream.Length > 0
        ? stream.ToArray()
        : throw new InvalidOperationException($"cannot encode {label} as PNG");
}

// ICO is a tiny container; SkiaSharp has no ICO encoder, so the directory and
// header are assembled here around ordinary PNG frames (valid since Vista and
// understood by every browser that supports favicons).
static byte[] BuildIco(SKBitmap source, IReadOnlyList<int> sizes)
{
    var frames = new List<byte[]>(sizes.Count);
    foreach (var size in sizes)
    {
        using var frame = Resize(source, size, size);
        frames.Add(EncodePng(frame, $"favicon {size}"));
    }

    using var stream = new MemoryStream();
    using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true);
    writer.Write((ushort)0);              // reserved
    writer.Write((ushort)1);              // 1 = icon
    writer.Write((ushort)frames.Count);
    var offset = 6 + 16 * frames.Count;
    for (var index = 0; index < frames.Count; index++)
    {
        var size = sizes[index];
        writer.Write((byte)(size >= 256 ? 0 : size));
        writer.Write((byte)(size >= 256 ? 0 : size));
        writer.Write((byte)0);            // palette colours
        writer.Write((byte)0);            // reserved
        writer.Write((ushort)1);          // colour planes
        writer.Write((ushort)32);         // bits per pixel
        writer.Write(frames[index].Length);
        writer.Write(offset);
        offset += frames[index].Length;
    }
    foreach (var frame in frames)
        writer.Write(frame);
    writer.Flush();
    return stream.ToArray();
}

static byte[] BuildSocialPreview(string screenshotPath, string fontPath)
{
    using var social = new SKBitmap(new SKImageInfo(SocialWidth, SocialHeight, SKColorType.Rgba8888, SKAlphaType.Premul));
    using (var canvas = new SKCanvas(social))
    {
        canvas.Clear(new SKColor(0xf6, 0xf7, 0xfb));
        using var typeface = SKTypeface.FromFile(fontPath)
            ?? throw new InvalidDataException($"cannot load {fontPath}");
        using var font = new SKFont(typeface, 32)
        {
            Subpixel = true,
            Edging = SKFontEdging.Antialias,
        };
        using var paint = new SKPaint
        {
            Color = new SKColor(0x18, 0x23, 0x36),
            IsAntialias = true,
        };
        // Pillow's default "la" anchor puts y at the ascender line; translate the
        // ascender distance into the baseline Skia draws from.
        var baseline = 16f - font.Metrics.Ascent;
        canvas.DrawText(BrandText, 40f, baseline, SKTextAlign.Left, font, paint);

        using var screenshot = Decode(screenshotPath);
        var scale = Math.Min(
            (double)ScreenshotWidth / screenshot.Width,
            (double)ScreenshotHeight / screenshot.Height);
        var width = (int)Math.Round(screenshot.Width * scale);
        var height = (int)Math.Round(screenshot.Height * scale);
        var destination = SKRect.Create(
            (SocialWidth - width) / 2f,
            82f,
            width,
            height);
        canvas.DrawBitmap(screenshot, destination, new SKSamplingOptions(SKCubicResampler.Mitchell));
    }
    // Share images never need transparency: Pillow wrote RGB, and keeping the
    // alpha channel out avoids paying for it in the PNG stream.
    using var opaque = new SKBitmap(new SKImageInfo(SocialWidth, SocialHeight, SKColorType.Rgb888x, SKAlphaType.Opaque));
    using (var canvas = new SKCanvas(opaque))
    {
        canvas.DrawBitmap(
            social,
            SKRect.Create(0, 0, SocialWidth, SocialHeight),
            new SKSamplingOptions(SKFilterMode.Nearest, SKMipmapMode.None));
    }
    return EncodePng(opaque, "images/social-preview.png");
}

static byte[] BrotliCompress(byte[] source)
{
    var destination = new byte[BrotliEncoder.GetMaxCompressedLength(source.Length)];
    if (!BrotliEncoder.TryCompress(source, destination, out var written, quality: 11, window: 22))
        throw new InvalidOperationException("Brotli compression failed");
    return destination[..written];
}
