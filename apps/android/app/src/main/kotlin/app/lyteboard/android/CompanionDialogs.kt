package app.lyteboard.android

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import app.lyteboard.core.Connection
import app.lyteboard.core.Device
import app.lyteboard.core.Kind
import app.lyteboard.core.PairState
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

@Composable
internal fun PairDialog(model: PeerModel, scan: () -> Unit, close: () -> Unit) {
    var invitation by remember { mutableStateOf("") }
    var verification by remember { mutableStateOf("") }
    val state = model.snapshot?.pairing
    fun cancel() {
        model.operation { it.cancelPairing() }
        close()
    }
    AlertDialog(
        onDismissRequest = ::cancel,
        title = { Text("Pair a device") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                when (state) {
                    PairState.Connecting -> Text("Contacting the Linux device…")
                    PairState.WaitingForPeer -> Text("Waiting for approval on Linux.")
                    PairState.Complete -> Text("Device paired. Lyteboard is opening the secure connection.")
                    is PairState.Compare -> {
                        Text("Compare this code with the one shown on Linux.")
                        Text(state.code, style = MaterialTheme.typography.headlineSmall,
                            color = MaterialTheme.colorScheme.primary)
                        OutlinedTextField(verification, { if (it.length <= 19) verification = it },
                            label = { Text("Code shown on Linux") }, singleLine = true)
                    }
                    else -> {
                        if (state is PairState.Failed) Text(state.message, color = MaterialTheme.colorScheme.error)
                        Text("Open a pairing invitation in Lyteboard on Linux.")
                        Button(onClick = scan, modifier = Modifier.fillMaxWidth()) { Text("Scan QR code") }
                        OutlinedTextField(
                            invitation, { if (it.length <= 512) invitation = it },
                            label = { Text("Paste invitation") }, singleLine = true,
                            visualTransformation = PasswordVisualTransformation(),
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                        )
                    }
                }
            }
        },
        confirmButton = {
            when (state) {
                is PairState.Compare -> Button(onClick = {
                    val code = verification.trim()
                    verification = ""
                    model.operation { it.confirmPairing(code) }
                }, enabled = verification.isNotBlank()) { Text("Approve") }
                PairState.Complete -> TextButton(onClick = ::cancel) { Text("Done") }
                PairState.Connecting, PairState.WaitingForPeer -> Unit
                else -> TextButton(onClick = {
                    val value = invitation.trim()
                    invitation = ""
                    model.operation { it.importInvitation(value) }
                }, enabled = invitation.isNotBlank()) { Text("Import invitation") }
            }
        },
        dismissButton = {
            if (state != PairState.Complete) TextButton(onClick = ::cancel) { Text("Cancel") }
        },
    )
}

@Composable
internal fun WriteDialog(close: () -> Unit, done: (Content) -> Unit) {
    var value by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    AlertDialog(
        onDismissRequest = close,
        title = { Text("Write text") },
        text = {
            OutlinedTextField(
                value, { if (it.length <= TEXT_LIMIT) value = it },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Message") },
                minLines = 3,
                maxLines = 6,
                isError = error != null,
                supportingText = { error?.let { Text(it) } },
            )
        },
        confirmButton = {
            Button(onClick = {
                runCatching { textContent(value) }.onSuccess(done).onFailure { error = it.message }
            }, enabled = value.isNotEmpty()) { Text("Choose recipients") }
        },
        dismissButton = { TextButton(onClick = close) { Text("Cancel") } },
    )
}

@Composable
internal fun RecipientsDialog(
    content: Content,
    devices: List<Device>,
    initial: Set<String>,
    busy: Boolean,
    close: () -> Unit,
    send: (Set<String>) -> Unit,
) {
    var chosen by remember(content) { mutableStateOf(initial) }
    val available = devices.filter { it.connection == Connection.CONNECTED }.map(Device::id).toSet()
    val recipients = chosen.intersect(available)
    AlertDialog(
        onDismissRequest = close,
        title = { Text("Send ${content.label.lowercase()}") },
        text = {
            Column(Modifier.heightIn(max = 340.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(10.dp)) {
                if (content.kind == Kind.TEXT) Text(content.bytes.decodeToString(endIndex = minOf(512, content.bytes.size)),
                    maxLines = 3, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis)
                if (available.isEmpty()) {
                    Text("Open Lyteboard on a paired Linux device to send.")
                } else {
                    Text("Choose one or more online devices.")
                    for (peer in devices) {
                        val online = peer.id in available
                        val checked = online && peer.id in chosen
                        Row(
                            Modifier.fillMaxWidth()
                                .toggleable(checked, enabled = online, role = Role.Checkbox) {
                                    chosen = if (it) chosen + peer.id else chosen - peer.id
                                }.padding(vertical = 8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            Checkbox(checked, onCheckedChange = null, enabled = online)
                            Text(if (online) peer.label else "${peer.label} · offline")
                        }
                    }
                }
            }
        },
        confirmButton = {
            Button(onClick = { send(recipients) }, enabled = recipients.isNotEmpty() && !busy) {
                Text(if (recipients.isEmpty()) "Send" else "Send to ${recipients.size}")
            }
        },
        dismissButton = { TextButton(onClick = close) { Text("Cancel") } },
    )
}

@Composable
internal fun PreviewDialog(
    content: Content,
    close: () -> Unit,
    canDelete: Boolean,
    copy: () -> Unit,
    save: () -> Unit,
    resend: () -> Unit,
    delete: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = close,
        title = { Text("${content.label} preview") },
        text = {
            Column(Modifier.heightIn(max = 380.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (content.kind == Kind.TEXT) {
                    Text(content.bytes.decodeToString(endIndex = minOf(content.bytes.size, 4096)))
                    if (content.bytes.size > 4096) Text("Copy or save to use the full text.")
                } else {
                    val bitmap by produceState<android.graphics.Bitmap?>(null, content) {
                        value = withContext(Dispatchers.IO) { runCatching { previewBitmap(content) }.getOrNull() }
                    }
                    bitmap?.let { Image(it.asImageBitmap(), contentDescription = "Received image",
                        modifier = Modifier.fillMaxWidth().heightIn(max = 240.dp)) }
                        ?: Text("${content.label} · ${content.bytes.size} bytes")
                }
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    OutlinedButton(onClick = copy) { Text("Copy") }
                    OutlinedButton(onClick = save) { Text("Save") }
                    OutlinedButton(onClick = resend) { Text("Resend") }
                    if (canDelete) TextButton(onClick = delete) { Text("Delete") }
                }
            }
        },
        confirmButton = { TextButton(onClick = close) { Text("Done") } },
    )
}

@Composable
internal fun ConnectionsDialog(model: PeerModel, devices: List<Device>, close: () -> Unit) {
    var selected by remember { mutableStateOf(devices.firstOrNull()?.id) }
    val device = devices.firstOrNull { it.id == selected }
    var address by remember(device?.id) { mutableStateOf(device?.address.orEmpty()) }
    var remove by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = close,
        title = { Text("Device connections") },
        text = {
            Column(Modifier.heightIn(max = 360.dp).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (devices.isEmpty()) Text("No paired devices.")
                devices.forEach { peer ->
                    TextButton(onClick = { selected = peer.id }, enabled = selected != peer.id) { Text(peer.label) }
                }
                if (device != null) {
                    HorizontalDivider()
                    Text("Manual connection", style = MaterialTheme.typography.titleSmall)
                    Text("Enter the IP address and TCP port shown by Lyteboard on Linux.")
                    OutlinedTextField(address, { if (it.length <= 64) address = it }, singleLine = true,
                        label = { Text("IP address and port") }, placeholder = { Text("192.168.1.20:47851") })
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(onClick = {
                            val value = address.trim()
                            model.connectDevice(device.id, value)
                            close()
                        }, enabled = address.isNotBlank()) { Text("Connect") }
                        TextButton(onClick = { remove = true }) { Text("Remove") }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = close) { Text("Done") } },
    )
    if (remove && device != null) AlertDialog(
        onDismissRequest = { remove = false },
        title = { Text("Remove ${device.label}?") },
        text = { Text("This revokes its access and closes the connection.") },
        confirmButton = {
            TextButton(onClick = {
                model.operation { it.removeDevice(device.id) }
                remove = false
                close()
            }) { Text("Remove device") }
        },
        dismissButton = { TextButton(onClick = { remove = false }) { Text("Cancel") } },
    )
}

@Composable
internal fun RetentionDialog(initial: Retention, close: () -> Unit, save: (Retention) -> Unit) {
    var count by remember { mutableStateOf(initial.entries.toString()) }
    var mib by remember { mutableStateOf(initial.mib.toString()) }
    var days by remember { mutableStateOf(initial.days.toString()) }
    val value = runCatching { Retention(count.toInt(), mib.toInt(), days.toInt()) }.getOrNull()
    AlertDialog(
        onDismissRequest = close,
        title = { Text("History retention") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Old items are deleted as limits are reached.")
                LimitField("Items (1–500)", count) { count = it }
                LimitField("Total content in MiB (1–256)", mib) { mib = it }
                LimitField("Days (1–365)", days) { days = it }
            }
        },
        confirmButton = {
            Button(onClick = { value?.let(save) }, enabled = value != null) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = close) { Text("Cancel") } },
    )
}

@Composable
private fun LimitField(title: String, value: String, change: (String) -> Unit) =
    OutlinedTextField(value, { if (it.length <= 3 && it.all(Char::isDigit)) change(it) },
        label = { Text(title) }, singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number))
