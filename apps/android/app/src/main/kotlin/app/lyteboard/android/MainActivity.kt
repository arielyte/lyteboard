package app.lyteboard.android

import android.app.Application
import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import app.lyteboard.core.Connection
import app.lyteboard.core.Mobile
import app.lyteboard.core.Snapshot
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

class MainActivity : ComponentActivity() {
    private val model by viewModels<PeerModel>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent { CompanionScreen(model) }
        receiveShare(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        receiveShare(intent)
    }

    private fun receiveShare(incoming: Intent) {
        if (incoming.action == Intent.ACTION_SEND) {
            model.prepare { readShared(applicationContext, incoming) }
            setIntent(Intent(this, MainActivity::class.java).setAction(Intent.ACTION_MAIN))
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

internal enum class Appearance(val label: String) { SYSTEM("System"), LIGHT("Light"), DARK("Dark") }

internal class PeerModel(application: Application) : AndroidViewModel(application) {
    private val app = application
    private val settings = app.getSharedPreferences("companion", 0)
    private val history = LocalHistory(app)
    private var watcher: LanChanges? = null
    private var discovery: Discovery? = null
    private var refresh: Job? = null
    private var sending: Job? = null
    private var opening = false
    private var visible = false

    var mobile by mutableStateOf<Mobile?>(null)
        private set
    var snapshot by mutableStateOf<Snapshot?>(null)
        private set
    var openingError by mutableStateOf<String?>(null)
        private set
    var feedback by mutableStateOf<String?>(null)
        private set
    var draft by mutableStateOf<Content?>(null)
        private set
    var busy by mutableStateOf(false)
        private set
    var historyRows by mutableStateOf(emptyList<HistoryRow>())
        private set
    var historyError by mutableStateOf<String?>(null)
        private set
    var historyEnabled by mutableStateOf(settings.getBoolean("history", false))
        private set
    var discover by mutableStateOf(settings.getBoolean("discovery", true))
        private set
    var appearance by mutableStateOf(
        Appearance.entries.firstOrNull { it.name == settings.getString("theme", null) } ?: Appearance.SYSTEM,
    )
        private set
    var retention by mutableStateOf(Retention(
        settings.getInt("count", 100).coerceIn(1, 500),
        settings.getInt("mib", 100).coerceIn(1, 256),
        settings.getInt("days", 7).coerceIn(1, 365),
    ))
        private set

    init { open() }

    fun open() {
        if (opening || mobile != null) return
        opening = true
        openingError = null
        viewModelScope.launch {
            try {
                val endpoint = withContext(Dispatchers.IO) {
                    CredentialProtection(app.noBackupFilesDir).open("Lyteboard Android")
                }
                mobile = endpoint
                endpoint.setHistoryEnabled(historyEnabled)
                if (visible) start(endpoint)
            } catch (error: CancellationException) {
                throw error
            } catch (failure: Exception) {
                openingError = failure.message ?: "Could not open your protected device profile."
            } catch (_: LinkageError) {
                openingError = "Could not load the Lyteboard library."
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
            watcher?.close()
            watcher = null
            refresh?.cancel()
            refresh = null
            mobile?.setActive(false)
            if (busy) {
                sending?.cancel()
                report("Sending stopped when Lyteboard left the foreground.")
            }
        }
    }

    private fun start(endpoint: Mobile) {
        endpoint.setActive(true)
        if (watcher == null) watcher = LanChanges(app) {
            endpoint.networkChanged()
            updateDiscovery(endpoint)
        }
        updateDiscovery(endpoint)
        refreshHistory()
        if (refresh == null) refresh = viewModelScope.launch {
            var ticks = 0
            while (true) {
                safely {
                    snapshot = withContext(Dispatchers.IO) { endpoint.snapshot() }
                    if (historyEnabled) {
                        val limits = retention
                        historySafely {
                            val rows = withContext(Dispatchers.IO) {
                                var changed = false
                                repeat(4) {
                                    val item = endpoint.nextHistoryItem() ?: return@repeat
                                    try {
                                        history.add(item, limits)
                                    } finally {
                                        item.bytes.fill(0)
                                    }
                                    changed = true
                                }
                                // Enforce age retention even on idle foreground sessions.
                                if (++ticks >= 120) {
                                    history.cleanup(limits)
                                    ticks = 0
                                    changed = true
                                }
                                if (changed) history.list() else null
                            }
                            rows?.let { historyRows = it }
                        }
                    }
                }
                delay(500)
            }
        }
    }

    private fun updateDiscovery(endpoint: Mobile) {
        discovery?.close()
        discovery = if (visible && discover) Discovery(app, endpoint::discoveryHint).also { it.start() } else null
    }

    fun setDiscovery(enabled: Boolean) {
        discover = enabled
        settings.edit().putBoolean("discovery", enabled).apply()
        mobile?.let(::updateDiscovery)
    }

    fun changeAppearance(value: Appearance) {
        appearance = value
        settings.edit().putString("theme", value.name).apply()
    }

    fun setHistory(enabled: Boolean) {
        historyEnabled = enabled
        mobile?.setHistoryEnabled(enabled)
        settings.edit().putBoolean("history", enabled).apply()
        refreshHistory()
    }

    fun saveRetention(value: Retention) {
        retention = value
        settings.edit().putInt("count", value.entries).putInt("mib", value.mib).putInt("days", value.days).apply()
        refreshHistory()
    }

    private fun refreshHistory() {
        if (!historyEnabled) {
            historyRows = emptyList()
            historyError = null
            return
        }
        val limits = retention
        viewModelScope.launch {
            historySafely {
                val rows = withContext(Dispatchers.IO) { history.cleanup(limits); history.list() }
                if (historyEnabled) historyRows = rows
            }
        }
    }

    fun delete(row: HistoryRow) = changeHistory { history.delete(row.id) }
    fun clearHistory() = changeHistory { history.clear() }

    private fun changeHistory(operation: () -> Unit) {
        viewModelScope.launch {
            historySafely {
                withContext(Dispatchers.IO) { operation() }
                refreshHistory()
            }
        }
    }

    private suspend fun historySafely(action: suspend () -> Unit) {
        try {
            action()
            historyError = null
        } catch (error: CancellationException) {
            throw error
        } catch (_: Exception) {
            historyError = "Could not access local history."
        }
    }

    fun operation(action: (Mobile) -> Unit) {
        val endpoint = mobile ?: return report("Wait for the device profile to open.")
        viewModelScope.launch {
            safely { withContext(Dispatchers.IO) { action(endpoint) } }
        }
    }

    fun connectDevice(id: String, address: String) {
        val endpoint = mobile ?: return
        viewModelScope.launch {
            safely {
                val connected = withContext(Dispatchers.IO) {
                    val canonical = endpoint.connect(id, address)
                    withTimeoutOrNull(12_000) {
                        while (endpoint.snapshot().devices.none {
                            it.id == id && it.connection == Connection.CONNECTED && it.address == canonical
                        }) delay(200)
                        true
                    }
                }
                if (visible) report(if (connected == true) "Device connected." else
                    "Could not connect. Check the Linux listener IP and port, the TCP firewall rule, and Wi-Fi client isolation.")
            }
        }
    }

    fun prepare(loader: () -> Content) {
        viewModelScope.launch {
            safely {
                draft = withContext(Dispatchers.IO) { loader() }
            }
        }
    }

    fun clearDraft() { draft = null }

    fun openContent(sequence: ULong, kind: app.lyteboard.core.Kind, done: (Content) -> Unit) {
        val endpoint = mobile ?: return
        viewModelScope.launch {
            safely { done(withContext(Dispatchers.IO) { Content(kind, endpoint.receivedBytes(sequence)) }) }
        }
    }

    fun openHistory(row: HistoryRow, done: (Content) -> Unit) {
        viewModelScope.launch {
            safely { done(withContext(Dispatchers.IO) { history.load(row) }) }
        }
    }

    fun share(content: Content) { draft = content }

    fun send(content: Content, recipients: Set<String>) {
        val endpoint = mobile ?: return
        if (sending?.isActive == true || recipients.isEmpty()) return
        busy = true
        draft = null
        sending = viewModelScope.launch {
            var delivered = 0
            var lastFailure: String? = null
            try {
                for ((index, id) in recipients.withIndex()) {
                    if (!visible) break
                    val status = withContext(Dispatchers.IO) {
                        try {
                            check(endpoint.snapshot().devices.any { it.id == id && it.connection == Connection.CONNECTED }) {
                                "The device is offline."
                            }
                            endpoint.send(id, content.kind, content.bytes)
                            withTimeoutOrNull(75_000) {
                                var value = endpoint.snapshot().sendStatus
                                while (value == "Sending…") {
                                    delay(150)
                                    value = endpoint.snapshot().sendStatus
                                }
                                value
                            } ?: "Delivery timed out."
                        } catch (error: CancellationException) {
                            throw error
                        } catch (failure: Exception) {
                            failure.message ?: "Transfer failed."
                        }
                    }
                    if (status == "Delivered.") delivered++ else lastFailure = status.ifBlank { "Connection closed." }
                    if (index + 1 < recipients.size) report("Sending to ${index + 2} of ${recipients.size}…")
                }
                val missing = recipients.size - delivered
                report(when {
                    missing == 0 -> "Delivered to $delivered ${if (delivered == 1) "device" else "devices"}."
                    delivered == 0 -> lastFailure ?: "Delivery stopped when Lyteboard left the foreground."
                    else -> "Delivered to $delivered of ${recipients.size} devices. ${lastFailure.orEmpty()}".trim()
                })
            } finally {
                busy = false
            }
        }
    }

    fun report(value: String?) { feedback = value }

    private suspend fun safely(action: suspend () -> Unit) {
        try {
            action()
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            report(error.message ?: "Could not complete the operation.")
        }
    }

    override fun onCleared() {
        foregroundChanged(false)
        mobile?.close()
        history.close()
    }
}
