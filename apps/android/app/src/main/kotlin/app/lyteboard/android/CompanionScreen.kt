package app.lyteboard.android

import android.app.Activity
import android.content.Intent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.lyteboard.core.Connection
import app.lyteboard.core.Device
import app.lyteboard.core.Kind
import java.text.DateFormat
import java.util.Date

private enum class Page(val label: String) { DEVICES("Devices"), HISTORY("History"), SETTINGS("Settings") }

@Composable
internal fun CompanionScreen(model: PeerModel) = CompanionTheme(model.appearance) {
    val context = LocalContext.current
    val notices = remember { SnackbarHostState() }
    var page by rememberSaveable { mutableStateOf(Page.DEVICES) }
    var pair by remember { mutableStateOf(false) }
    var scan by remember { mutableStateOf(false) }
    var compose by remember { mutableStateOf(false) }
    var advanced by remember { mutableStateOf(false) }
    var clear by remember { mutableStateOf(false) }
    var limits by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf(emptySet<String>()) }
    var preview by remember { mutableStateOf<Content?>(null) }
    var previewRow by remember { mutableStateOf<HistoryRow?>(null) }
    var saving by remember { mutableStateOf<Content?>(null) }
    val snapshot = model.snapshot
    val devices = snapshot?.devices.orEmpty()
    val online = devices.filter { it.connection == Connection.CONNECTED }

    LaunchedEffect(model.feedback) {
        model.feedback?.let {
            notices.showSnackbar(it)
            model.report(null)
        }
    }
    LaunchedEffect(devices.map(Device::id)) {
        selected = selected.intersect(devices.map(Device::id).toSet())
    }

    val image = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) model.prepare { readImage(context, uri) }
    }
    val save = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val item = saving
        val uri = result.data?.data
        saving = null
        if (item != null && result.resultCode == Activity.RESULT_OK && uri != null) {
            model.operation {
                context.contentResolver.openOutputStream(uri)?.use { it.write(item.bytes) }
                    ?: error("Could not write the selected file.")
            }
        }
    }

    Scaffold(
        modifier = Modifier.fillMaxSize(),
        containerColor = MaterialTheme.colorScheme.background,
        topBar = { Header(page, online.size, model.busy) },
        snackbarHost = { SnackbarHost(notices) },
        bottomBar = {
            NavigationBar(containerColor = MaterialTheme.colorScheme.surface) {
                for (item in Page.entries) {
                    NavigationBarItem(
                        selected = item == page,
                        onClick = { page = item },
                        icon = { PageIcon(item) },
                        label = { Text(item.label) },
                    )
                }
            }
        },
    ) { insets ->
        LazyColumn(
            modifier = Modifier.padding(insets).fillMaxSize().imePadding(),
            contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 12.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            when (page) {
                Page.DEVICES -> {
                    item {
                        Panel {
                            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween,
                                verticalAlignment = Alignment.CenterVertically) {
                                Column(Modifier.weight(1f)) {
                                    Text("Paired devices", style = MaterialTheme.typography.titleLarge)
                                    Muted(if (devices.isEmpty()) "Connect Android to Lyteboard on Linux." else
                                        "${devices.size} paired · ${online.size} online")
                                }
                                Button(onClick = { pair = true }, enabled = snapshot != null) { Text("Pair device") }
                            }
                        }
                    }
                    if (model.openingError != null) item {
                        Panel {
                            Text(model.openingError.orEmpty(), color = MaterialTheme.colorScheme.error)
                            TextButton(onClick = model::open) { Text("Retry") }
                        }
                    } else if (snapshot == null) item { Muted("Opening your protected device profile…") }
                    items(devices, key = { it.id }) { device ->
                        DeviceCard(device, device.id in selected) {
                            selected = if (device.id in selected) selected - device.id else selected + device.id
                        }
                    }
                    item {
                        Panel {
                            Text("Send content", style = MaterialTheme.typography.titleMedium)
                            Muted(when {
                                model.busy -> "Sending securely over the local network…"
                                selected.isNotEmpty() -> "${selected.size} selected"
                                online.isNotEmpty() -> "Choose recipients before sending."
                                else -> "A paired device needs to be online."
                            })
                            Button(
                                onClick = {
                                    runCatching { foregroundClip(context) }
                                        .onSuccess { clip -> model.prepare { readClipboard(context, clip) } }
                                        .onFailure { model.report(it.message ?: "Clipboard access is unavailable.") }
                                },
                                enabled = online.isNotEmpty() && !model.busy,
                                modifier = Modifier.fillMaxWidth(),
                            ) { Text("Send clipboard") }
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                OutlinedButton(onClick = { compose = true }, enabled = online.isNotEmpty() && !model.busy,
                                    modifier = Modifier.weight(1f)) { Text("Write text") }
                                OutlinedButton(onClick = { image.launch(IMAGE_TYPES) }, enabled = online.isNotEmpty() && !model.busy,
                                    modifier = Modifier.weight(1f)) { Text("Choose image") }
                            }
                        }
                    }
                    snapshot?.latest?.let { latest ->
                        item {
                            Panel {
                                Text("Latest received", style = MaterialTheme.typography.titleMedium)
                                val name = devices.firstOrNull { it.id == latest.deviceId }?.label ?: "Paired device"
                                Muted("$name · ${latest.kind.label()} · ${sizeLabel(latest.size.toInt())}")
                                OutlinedButton(onClick = {
                                    model.openContent(latest.sequence, latest.kind) { previewRow = null; preview = it }
                                }) { Text("Preview") }
                            }
                        }
                    }
                }

                Page.HISTORY -> {
                    if (model.historyError != null) item { Panel { Text(model.historyError.orEmpty()) } }
                    if (!model.historyEnabled) item {
                        Panel {
                            Text("History is off", style = MaterialTheme.typography.titleLarge)
                            Muted("Enable local history in Settings to keep sent and received content.")
                            OutlinedButton(onClick = { page = Page.SETTINGS }) { Text("Open settings") }
                        }
                    } else {
                        if (model.historyRows.isEmpty()) item {
                            Panel {
                                Text("No history yet", style = MaterialTheme.typography.titleLarge)
                                Muted("Successful transfers appear here.")
                            }
                        } else item {
                            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween,
                                verticalAlignment = Alignment.CenterVertically) {
                                Muted("${model.historyRows.size} saved items")
                                TextButton(onClick = { clear = true }) { Text("Clear history") }
                            }
                        }
                        items(model.historyRows, key = { it.id }) { row ->
                            val peer = devices.firstOrNull { it.id == row.peer }?.label ?: "Former device"
                            val time = remember(row.timestamp) { DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT)
                                .format(Date(row.timestamp * 1000)) }
                            Surface(
                                onClick = { model.openHistory(row) { previewRow = row; preview = it } },
                                shape = MaterialTheme.shapes.large,
                                border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
                            ) {
                                Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                    Text("${if (row.sent) "Sent to" else "Received from"} $peer",
                                        style = MaterialTheme.typography.titleSmall)
                                    Muted("${row.kind.label()} · ${sizeLabel(row.size)}")
                                    Muted(time)
                                }
                            }
                        }
                    }
                }

                Page.SETTINGS -> {
                    item {
                        Panel {
                            Text("Appearance", style = MaterialTheme.typography.titleMedium)
                            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                for (value in Appearance.entries) {
                                    FilterChip(selected = model.appearance == value, onClick = { model.changeAppearance(value) },
                                        label = { Text(value.label) })
                                }
                            }
                        }
                    }
                    item {
                        Panel {
                            Toggle("Local history", "Save sent and received items on this phone.", model.historyEnabled, model::setHistory)
                            Muted("History is stored in the app’s private storage without encryption at rest.")
                            HorizontalDivider()
                            Muted("${model.retention.entries} items · ${model.retention.mib} MiB · ${model.retention.days} days")
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                OutlinedButton(onClick = { limits = true }) { Text("Retention") }
                                TextButton(onClick = { clear = true }) { Text("Clear history") }
                            }
                        }
                    }
                    item {
                        Panel {
                            Toggle("LAN discovery", "Find updated addresses for devices you already trust.", model.discover, model::setDiscovery)
                            OutlinedButton(onClick = { advanced = true }, enabled = devices.isNotEmpty()) { Text("Manage device connections") }
                            Muted("Transfers are encrypted end to end and stay on the local network. Lyteboard connects while open.")
                        }
                    }
                }
            }
        }
    }

    if (pair && !scan) PairDialog(model, scan = { scan = true }, close = { pair = false })
    if (scan) ScanInvitation(
        onResult = { value -> scan = false; model.operation { it.importInvitation(value) } },
        onClose = { scan = false },
    )
    if (compose) WriteDialog({ compose = false }) { content -> model.share(content); compose = false }

    model.draft?.let { draft ->
        RecipientsDialog(draft, devices, selected, model.busy, model::clearDraft) { recipients ->
            selected = recipients
            model.send(draft, recipients)
        }
    }

    preview?.let { content ->
        PreviewDialog(
            content, close = { preview = null }, canDelete = previewRow != null,
            copy = {
                model.operation { copyContent(context, content) }
                preview = null
            },
            save = {
                saving = content
                preview = null
                save.launch(Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                    .setType(content.kind.mime).putExtra(Intent.EXTRA_TITLE, "lyteboard.${content.kind.extension}"))
            },
            resend = {
                preview = null
                model.share(content)
            },
            delete = {
                previewRow?.let(model::delete)
                preview = null
            },
        )
    }

    if (advanced) ConnectionsDialog(model, devices) { advanced = false }
    if (limits) RetentionDialog(model.retention, { limits = false }) { value ->
        model.saveRetention(value)
        limits = false
    }
    if (clear) AlertDialog(
        onDismissRequest = { clear = false },
        title = { Text("Clear all history?") },
        text = { Text("This deletes saved sent and received items from this phone.") },
        confirmButton = { TextButton(onClick = { model.clearHistory(); clear = false }) { Text("Clear history") } },
        dismissButton = { TextButton(onClick = { clear = false }) { Text("Cancel") } },
    )
}

@Composable
private fun Header(page: Page, online: Int, busy: Boolean) {
    Surface(color = MaterialTheme.colorScheme.surface) {
        Row(
            Modifier.fillMaxWidth().statusBarsPadding().padding(horizontal = 20.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Image(painterResource(R.drawable.lyteboard_icon), contentDescription = null, modifier = Modifier.size(36.dp))
            Column(Modifier.weight(1f)) {
                Text("Lyteboard", style = MaterialTheme.typography.titleLarge)
                Muted(page.label)
            }
            if (busy || online > 0) Surface(color = MaterialTheme.colorScheme.primaryContainer, shape = CircleShape) {
                Text(if (busy) "Sending…" else "$online online", Modifier.padding(horizontal = 11.dp, vertical = 5.dp),
                    color = MaterialTheme.colorScheme.onPrimaryContainer, style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

@Composable
private fun PageIcon(page: Page) {
    val color = androidx.compose.material3.LocalContentColor.current
    Canvas(Modifier.size(24.dp)) {
        val w = size.width / 24
        val stroke = Stroke(1.7f * w)
        when (page) {
            Page.DEVICES -> {
                drawRoundRect(color, Offset(2 * w, 5 * w), Size(14 * w, 11 * w), CornerRadius(2 * w), style = stroke)
                drawLine(color, Offset(5 * w, 19 * w), Offset(13 * w, 19 * w), stroke.width)
                drawRoundRect(color, Offset(16 * w, 10 * w), Size(6 * w, 10 * w), CornerRadius(1.5f * w), style = stroke)
            }
            Page.HISTORY -> {
                drawCircle(color, 8.5f * w, style = stroke)
                drawLine(color, Offset(12 * w, 7 * w), Offset(12 * w, 12 * w), stroke.width)
                drawLine(color, Offset(12 * w, 12 * w), Offset(16 * w, 14 * w), stroke.width)
            }
            Page.SETTINGS -> {
                for (position in listOf(6, 12, 18)) {
                    val x = if (position == 12) 16f else 8f
                    drawLine(color, Offset(3 * w, position * w), Offset(21 * w, position * w), stroke.width)
                    drawCircle(color, 2.4f * w, Offset(x * w, position * w))
                }
            }
        }
    }
}

@Composable
private fun Panel(content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit) {
    Surface(shape = MaterialTheme.shapes.large, border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant)) {
        Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(9.dp), content = content)
    }
}

@Composable
private fun Muted(value: String) {
    Text(value, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun Toggle(label: String, description: String, checked: Boolean, change: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        Column(Modifier.weight(1f)) {
            Text(label, style = MaterialTheme.typography.titleMedium)
            Muted(description)
        }
        Switch(checked = checked, onCheckedChange = change)
    }
}

@Composable
private fun DeviceCard(device: Device, selected: Boolean, select: () -> Unit) {
    Surface(onClick = select, shape = MaterialTheme.shapes.large,
        color = if (selected) MaterialTheme.colorScheme.primaryContainer else MaterialTheme.colorScheme.surface,
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant)) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 10.dp, vertical = 7.dp), verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = selected, onCheckedChange = null)
            Column(Modifier.padding(horizontal = 8.dp).weight(1f)) {
                Text(device.label.ifBlank { "Linux device" }, style = MaterialTheme.typography.titleMedium,
                    maxLines = 1, overflow = TextOverflow.Ellipsis)
                Muted(device.connection.label())
            }
            val color = if (device.connection == Connection.CONNECTED) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.outline
            Box(Modifier.padding(end = 8.dp).size(9.dp).background(color, CircleShape))
        }
    }
}

private fun Connection.label(): String = when (this) {
    Connection.CONNECTED -> "Online"
    Connection.CONNECTING -> "Connecting"
    Connection.OFFLINE -> "Offline"
}

private fun Kind.label(): String = if (this == Kind.TEXT) "Text" else name
private fun sizeLabel(bytes: Int): String = when {
    bytes >= 1024 * 1024 -> "${bytes / (1024 * 1024)} MiB"
    bytes >= 1024 -> "${bytes / 1024} KiB"
    else -> "$bytes bytes"
}
