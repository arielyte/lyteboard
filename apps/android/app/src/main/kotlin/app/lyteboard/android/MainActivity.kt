package app.lyteboard.android

import android.app.Application
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import app.lyteboard.core.Connection
import app.lyteboard.core.Kind
import app.lyteboard.core.Mobile
import app.lyteboard.core.PairState
import app.lyteboard.core.Snapshot
import app.lyteboard.core.coreInfo
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    private val model by viewModels<PeerModel>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            val colors = if (isSystemInDarkTheme()) darkColorScheme() else lightColorScheme()
            MaterialTheme(colorScheme = colors) {
                Scaffold { insets ->
                    Column(
                        modifier = Modifier.fillMaxSize().padding(insets).verticalScroll(rememberScrollState()).padding(20.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineMedium)
                        val versions = remember { coreInfo() }
                        val core = stringResource(R.string.core_version, versions.coreVersion)
                        val wire = stringResource(R.string.protocol_version, versions.protocolVersion.toInt())
                        Text("$core · $wire")
                        val mobile = model.mobile
                        if (mobile == null) {
                            Text(model.error ?: "Opening protected peer profile…")
                            if (model.error != null) Button(onClick = model::open) { Text("Retry") }
                        } else {
                            PeerControls(mobile, model.visible)
                        }
                    }
                }
            }
        }
    }

    override fun onStart() {
        super.onStart()
        model.foregroundChanged(true)
    }

    override fun onStop() {
        model.foregroundChanged(false)
        super.onStop()
    }
}

class PeerModel(application: Application) : AndroidViewModel(application) {
    var mobile by mutableStateOf<Mobile?>(null)
        private set
    var error by mutableStateOf<String?>(null)
        private set
    var visible by mutableStateOf(false)
        private set
    private var discovery: Discovery? = null
    private var opening = false

    init { open() }

    fun open() {
        if (opening || mobile != null) return
        opening = true
        error = null
        viewModelScope.launch {
            try {
                val endpoint = withContext(Dispatchers.IO) {
                    CredentialProtection(getApplication<Application>().noBackupFilesDir).open("Lyteboard Android")
                }
                mobile = endpoint
                if (visible) start(endpoint)
            } catch (error: CancellationException) {
                throw error
            } catch (failure: Exception) {
                error = failure.message ?: "Could not open the protected peer profile."
            } catch (_: LinkageError) {
                error = "Could not load the native Lyteboard library."
            } finally {
                opening = false
            }
        }
    }

    fun foregroundChanged(value: Boolean) {
        visible = value
        if (value) {
            mobile?.let(::start)
        } else {
            discovery?.close()
            discovery = null
            mobile?.setActive(false)
        }
    }

    private fun start(endpoint: Mobile) {
        endpoint.setActive(true)
        if (discovery == null) {
            discovery = Discovery(getApplication(), endpoint).also { it.start() }
        }
    }

    override fun onCleared() {
        foregroundChanged(false)
        mobile?.close()
    }
}

@Composable
private fun PeerControls(mobile: Mobile, visible: Boolean) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var state by remember { mutableStateOf<Snapshot?>(null) }
    var status by remember { mutableStateOf("") }
    var invitation by remember { mutableStateOf("") }
    var code by remember { mutableStateOf("") }
    var selected by remember { mutableStateOf("") }
    var address by remember { mutableStateOf("") }
    var message by remember { mutableStateOf("") }
    var preview by remember { mutableStateOf("") }
    val device = state?.devices?.firstOrNull { it.id == selected }
    val latest = state?.latest
    var saveSequence by remember { mutableStateOf<ULong?>(null) }

    fun run(operation: suspend () -> Unit) {
        scope.launch {
            status = ""
            try {
                withContext(Dispatchers.IO) { operation() }
                state = withContext(Dispatchers.IO) { mobile.snapshot() }
            } catch (error: kotlinx.coroutines.TimeoutCancellationException) {
                status = "The device did not reconnect."
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                status = error.message ?: "Operation failed."
            }
        }
    }

    val pickImage = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        val recipient = selected
        if (uri != null) run {
            val kind = when (context.contentResolver.getType(uri)) {
                "image/png" -> Kind.PNG
                "image/jpeg" -> Kind.JPEG
                "image/webp" -> Kind.WEBP
                else -> error("Choose a PNG, JPEG, or WebP image.")
            }
            val bytes = context.contentResolver.openInputStream(uri)?.use { it.readBounded(10 * 1024 * 1024) }
                ?: error("Could not open the selected image.")
            try {
                // The document picker can stop the activity and close its sessions.
                // Wait briefly for the same trusted device to reconnect on return.
                kotlinx.coroutines.withTimeout(8_000) {
                    while (mobile.snapshot().devices.any { it.id == recipient && it.connection != Connection.CONNECTED }) {
                        delay(200)
                    }
                    mobile.send(recipient, kind, bytes)
                }
            } finally {
                bytes.fill(0)
            }
        }
    }
    val save = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) { uri ->
        val sequence = saveSequence
        saveSequence = null
        if (uri != null && sequence != null) run {
            val bytes = mobile.receivedBytes(sequence)
            try {
                context.contentResolver.openOutputStream(uri)?.use { it.write(bytes) }
                    ?: error("Could not open the selected destination.")
            } finally {
                bytes.fill(0)
            }
        }
    }

    LaunchedEffect(mobile, visible) {
        if (!visible) {
            invitation = ""
            code = ""
            return@LaunchedEffect
        }
        while (true) {
            try {
                state = withContext(Dispatchers.IO) { mobile.snapshot() }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                status = error.message ?: "Could not read the peer state."
            }
            delay(600)
        }
    }
    LaunchedEffect(latest?.sequence) { preview = "" }

    Text("Pair with Linux", style = MaterialTheme.typography.titleMedium)
    OutlinedTextField(
        value = invitation, onValueChange = { if (it.length <= 512) invitation = it },
        modifier = Modifier.fillMaxWidth(), label = { Text("Pairing invitation") }, singleLine = true,
        visualTransformation = PasswordVisualTransformation(),
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
    )
    Button(onClick = {
        val payload = invitation.trim()
        invitation = ""
        run { mobile.importInvitation(payload) }
    }, enabled = invitation.isNotBlank()) { Text("Import invitation") }

    when (val pairing = state?.pairing) {
        is PairState.Compare -> {
            Text("Compare on both devices: ${pairing.code}")
            OutlinedTextField(
                value = code, onValueChange = { if (it.length <= 32) code = it },
                label = { Text("Code shown on Linux") }, singleLine = true,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = {
                    val compared = code.trim()
                    code = ""
                    run { mobile.confirmPairing(compared) }
                }, enabled = code.isNotBlank()) { Text("Approve") }
                Button(onClick = { code = ""; run { mobile.cancelPairing() } }) { Text("Cancel") }
            }
        }
        PairState.Connecting, PairState.WaitingForPeer -> {
            Text(if (pairing == PairState.Connecting) "Contacting Linux…" else "Waiting for Linux approval…")
            Button(onClick = { run { mobile.cancelPairing() } }) { Text("Cancel") }
        }
        PairState.Complete -> Text("Pairing complete.")
        is PairState.Failed -> Text(pairing.message)
        else -> Unit
    }

    Text("Trusted devices", style = MaterialTheme.typography.titleMedium)
    if (state?.devices.orEmpty().isEmpty()) Text("No paired devices.")
    state?.devices.orEmpty().forEach { item ->
        Button(onClick = { selected = item.id; address = item.address.orEmpty() }, enabled = selected != item.id) {
            Text("${item.label} · ${item.connection.name.lowercase()}")
        }
    }
    if (device != null) {
        OutlinedTextField(
            value = address, onValueChange = { if (it.length <= 64) address = it },
            label = { Text("IP and port") }, singleLine = true,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = {
                val hint = address.trim()
                run { mobile.connect(device.id, hint) }
            }, enabled = address.isNotBlank()) { Text("Connect") }
            Button(onClick = { run { mobile.removeDevice(device.id) } }) { Text("Remove device") }
        }
        OutlinedTextField(
            value = message, onValueChange = { if (it.length <= 1024 * 1024) message = it },
            modifier = Modifier.fillMaxWidth(), label = { Text("Test text") }, maxLines = 4,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = {
                val text = message
                run { mobile.send(device.id, Kind.TEXT, text.encodeToByteArray()) }
            }, enabled = device.connection == Connection.CONNECTED && message.isNotEmpty()) { Text("Send text") }
            Button(onClick = { pickImage.launch(arrayOf("image/png", "image/jpeg", "image/webp")) },
                enabled = device.connection == Connection.CONNECTED) { Text("Send image") }
        }
    }
    if (status.isNotBlank()) Text(status)
    state?.sendStatus?.takeIf { it.isNotBlank() }?.let { Text(it) }

    Text("Latest received", style = MaterialTheme.typography.titleMedium)
    if (latest == null) {
        Text("No transfers received.")
    } else {
        Text("${latest.kind.name.lowercase()} · ${latest.size} bytes")
        Text("SHA-256 ${latest.sha256}")
        if (latest.kind == Kind.TEXT) {
            Button(onClick = {
                scope.launch {
                    preview = try {
                        withContext(Dispatchers.IO) { mobile.receivedBytes(latest.sequence).decodeToString().take(4096) }
                    } catch (error: CancellationException) {
                        throw error
                    } catch (error: Exception) {
                        status = error.message ?: "Could not read the received message."
                        ""
                    }
                }
            }) { Text("View text") }
            if (preview.isNotEmpty()) Text(preview)
        }
        Button(onClick = {
            val extension = if (latest.kind == Kind.TEXT) "txt" else latest.kind.name.lowercase()
            saveSequence = latest.sequence
            save.launch("lyteboard-received.$extension")
        }) { Text("Save received item") }
    }
}
