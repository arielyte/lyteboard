package app.lyteboard.android

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.PersistableBundle
import androidx.core.content.FileProvider
import androidx.core.content.IntentCompat
import app.lyteboard.core.Kind
import java.io.File

internal const val IMAGE_LIMIT = 10 * 1024 * 1024
internal const val TEXT_LIMIT = 1024 * 1024
internal val IMAGE_TYPES = arrayOf("image/png", "image/jpeg", "image/webp")

internal val Kind.mime: String get() = when (this) {
    Kind.TEXT -> "text/plain"
    Kind.PNG -> "image/png"
    Kind.JPEG -> "image/jpeg"
    Kind.WEBP -> "image/webp"
}

internal val Kind.extension: String get() = if (this == Kind.TEXT) "txt" else name.lowercase()

internal data class Content(val kind: Kind, val bytes: ByteArray) {
    val label: String get() = if (kind == Kind.TEXT) "Text" else kind.name
}

internal fun textContent(value: CharSequence): Content {
    check(value.length <= TEXT_LIMIT) { "Text exceeds the 1 MiB limit." }
    val bytes = value.toString().encodeToByteArray()
    check(bytes.isNotEmpty() && bytes.size <= TEXT_LIMIT && 0.toByte() !in bytes) {
        "Use nonempty text without NUL characters, up to 1 MiB."
    }
    return Content(Kind.TEXT, bytes)
}

internal fun readImage(context: Context, uri: Uri, advertisedType: String? = null): Content {
    check(uri.scheme == "content") { "Choose an image from an Android file provider." }
    val providerType = context.contentResolver.getType(uri)
    val kind = when (providerType?.takeUnless { it == "application/octet-stream" } ?: advertisedType) {
        "image/png" -> Kind.PNG
        "image/jpeg" -> Kind.JPEG
        "image/webp" -> Kind.WEBP
        else -> error("Choose a PNG, JPEG, or WebP image.")
    }
    val bytes = context.contentResolver.openInputStream(uri)?.use { it.readBounded(IMAGE_LIMIT) }
        ?: error("Could not open the image.")
    check(bytes.isNotEmpty()) { "The image is empty." }
    return Content(kind, bytes)
}

internal fun readShared(context: Context, intent: Intent): Content {
    check(intent.action == Intent.ACTION_SEND) { "This share action is unsupported." }
    return if (intent.type == "text/plain") {
        val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT) ?: intent.clipData?.takeIf { it.itemCount > 0 }
            ?.getItemAt(0)?.text ?: error("No text was shared.")
        textContent(text)
    } else {
        val uri = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
            ?: intent.clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.uri
            ?: error("No image was shared.")
        readImage(context, uri, intent.type)
    }
}

internal fun foregroundClip(context: Context): ClipData =
    context.getSystemService(ClipboardManager::class.java).primaryClip
        ?.takeIf { it.itemCount > 0 } ?: error("No clipboard content is available. Keep Lyteboard in focus and try again.")

internal fun readClipboard(context: Context, clip: ClipData): Content {
    val item = clip.getItemAt(0)
    return item.text?.let(::textContent) ?: item.uri?.let { readImage(context, it) }
        ?: error("The clipboard has no supported text or image.")
}

internal fun copyContent(context: Context, content: Content) {
    val clipboard = context.getSystemService(ClipboardManager::class.java)
    val clip = if (content.kind == Kind.TEXT) {
        ClipData.newPlainText("Lyteboard", content.bytes.decodeToString())
    } else {
        val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
        val file = File.createTempFile("lyteboard-", ".${content.kind.extension}", directory)
        try {
            file.writeBytes(content.bytes)
            val uri = FileProvider.getUriForFile(context, "${context.packageName}.clipboard", file)
            ClipData(ClipDescription("Lyteboard", arrayOf(content.kind.mime)), ClipData.Item(uri))
        } catch (failure: Exception) {
            file.delete()
            throw failure
        }
    }
    clip.description.extras = PersistableBundle().apply {
        putBoolean("android.content.extra.IS_SENSITIVE", true)
    }
    clipboard.setPrimaryClip(clip)
    val directory = File(context.cacheDir, "clipboard")
    val current = clip.getItemAt(0).uri?.lastPathSegment
    directory.listFiles()?.filter { it.name != current }?.forEach { it.delete() }
}

internal fun previewBitmap(content: Content): Bitmap? {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeByteArray(content.bytes, 0, content.bytes.size, bounds)
    if (bounds.outWidth !in 1..8192 || bounds.outHeight !in 1..8192 ||
        bounds.outWidth.toLong() * bounds.outHeight > 16_000_000
    ) return null
    val max = maxOf(bounds.outWidth, bounds.outHeight)
    var sample = 1
    while (max / sample > 1200) sample *= 2
    return BitmapFactory.decodeByteArray(content.bytes, 0, content.bytes.size, BitmapFactory.Options().apply {
        inSampleSize = sample
    })
}
