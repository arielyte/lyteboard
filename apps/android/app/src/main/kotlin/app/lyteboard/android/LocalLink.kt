package app.lyteboard.android

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper
import app.lyteboard.core.LanAccess
import app.lyteboard.core.coreInfo
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.InetAddress
import java.net.NetworkInterface
import kotlin.math.min

internal fun InputStream.readBounded(limit: Int): ByteArray {
    val output = ByteArrayOutputStream()
    val buffer = ByteArray(8192)
    while (true) {
        val count = read(buffer, 0, min(buffer.size, limit + 1 - output.size()))
        if (count == -1) return output.toByteArray()
        check(output.size() + count <= limit) { "File exceeds the transfer limit." }
        output.write(buffer, 0, count)
    }
}

internal class LocalLink : LanAccess {
    override fun isOnLink(numericIp: String): Boolean = runCatching {
        // Rust supplies only a parsed numeric address. No name lookup is needed.
        val remote = InetAddress.getByName(numericIp)
        !remote.isLoopbackAddress && !remote.isAnyLocalAddress && !remote.isMulticastAddress &&
            NetworkInterface.getNetworkInterfaces().asSequence().any { network ->
                network.isUp && !network.isLoopback && !network.isPointToPoint &&
                    network.interfaceAddresses.any { local ->
                        val prefix = local.networkPrefixLength.toInt()
                        val address = local.address.address
                        val broadcast = local.broadcast?.address
                        val candidate = remote.address
                        prefix in 1..address.size * 8 && address.size == candidate.size &&
                            broadcast?.contentEquals(candidate) != true &&
                            address.indices.all { index ->
                                val bits = (prefix - index * 8).coerceIn(0, 8)
                                (address[index].toInt() xor candidate[index].toInt()) and
                                    (255 shl (8 - bits) and 255) == 0
                            }
                    }
            }
    }.getOrDefault(false)
}

@Suppress("DEPRECATION") // Legacy NSD resolution is available down to API 26.
internal class Discovery(context: Context, private val onHint: (String, String) -> Unit) : AutoCloseable {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private val localLink = LocalLink()
    private val wireVersion = coreInfo().protocolVersion.toString()
    private val pending = ArrayDeque<NsdServiceInfo>()
    private var started = false
    private var resolving = false
    private val monitor = if (android.os.Build.VERSION.SDK_INT >= 34)
        ServiceMonitor(nsd, context, ::publish) else null

    private val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(type: String) = Unit
        override fun onDiscoveryStopped(type: String) = Unit
        override fun onStartDiscoveryFailed(type: String, code: Int) = close()
        override fun onStopDiscoveryFailed(type: String, code: Int) = Unit
        override fun onServiceLost(service: NsdServiceInfo) {
            handler.post {
                if (android.os.Build.VERSION.SDK_INT >= 34) monitor?.remove(service)
            }
        }

        override fun onServiceFound(service: NsdServiceInfo) {
            handler.post {
                if (!started || !service.serviceName.startsWith("lyteboard-")) return@post
                if (android.os.Build.VERSION.SDK_INT >= 34) {
                    monitor?.track(service)
                } else if (pending.size < 16) {
                    pending.addLast(service)
                    resolveNext()
                }
            }
        }
    }

    fun start() {
        started = true
        runCatching { nsd.discoverServices("_lyteboard._tcp", NsdManager.PROTOCOL_DNS_SD, listener) }
            .onFailure { close() }
    }

    private fun resolveNext() {
        if (!started || resolving || pending.isEmpty()) return
        resolving = true
        runCatching {
            nsd.resolveService(pending.removeFirst(), object : NsdManager.ResolveListener {
                override fun onResolveFailed(service: NsdServiceInfo, code: Int) = resolved(null)
                override fun onServiceResolved(service: NsdServiceInfo) = resolved(service)
            })
        }.onFailure { resolved(null) }
    }

    private fun resolved(service: NsdServiceInfo?) {
        handler.post {
            resolving = false
            if (!started) return@post
            service?.let(::publish)
            resolveNext()
        }
    }

    private fun publish(service: NsdServiceInfo) {
        if (!started || service.attributes["v"]?.decodeToString() != wireVersion) return
        val pin = service.attributes["id"]?.decodeToString() ?: return
        if (pin.length != 64 || service.port !in 1..65535 ||
            !service.serviceName.equals("lyteboard-${pin.take(16)}", ignoreCase = true)
        ) return
        val hosts = if (android.os.Build.VERSION.SDK_INT >= 34) service.hostAddresses
            else listOfNotNull(service.host)
        val address = hosts.sortedBy { it.address.size }.firstOrNull { host ->
            host.hostAddress?.let { localLink.isOnLink(it) } == true
        }?.hostAddress ?: return
        val hint = if (':' in address) "[$address]:${service.port}" else "$address:${service.port}"
        onHint(pin, hint)
    }

    override fun close() {
        // Called from both NSD callbacks and the activity.
        if (Looper.myLooper() != Looper.getMainLooper()) {
            handler.post { close() }
            return
        }
        if (!started) return
        started = false
        pending.clear()
        if (android.os.Build.VERSION.SDK_INT >= 34) monitor?.close()
        runCatching { nsd.stopServiceDiscovery(listener) }
    }
}

@androidx.annotation.RequiresApi(34)
private class ServiceMonitor(
    private val nsd: NsdManager,
    context: Context,
    private val found: (NsdServiceInfo) -> Unit,
) {
    private val main = androidx.core.content.ContextCompat.getMainExecutor(context)
    private val subscriptions = mutableMapOf<Pair<String, android.net.Network?>, NsdManager.ServiceInfoCallback>()

    fun track(service: NsdServiceInfo) {
        val key = service.serviceName to service.network
        if (key in subscriptions || subscriptions.size >= 32) return
        val listener = object : NsdManager.ServiceInfoCallback {
            override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {
                subscriptions.remove(key, this)
            }

            override fun onServiceUpdated(serviceInfo: NsdServiceInfo) {
                if (subscriptions[key] === this) found(serviceInfo)
            }

            override fun onServiceLost() = Unit
            override fun onServiceInfoCallbackUnregistered() {
                subscriptions.remove(key, this)
            }
        }
        subscriptions[key] = listener
        runCatching { nsd.registerServiceInfoCallback(service, main, listener) }
            .onFailure { subscriptions.remove(key, listener) }
    }

    fun remove(service: NsdServiceInfo) {
        subscriptions.remove(service.serviceName to service.network)?.let {
            runCatching { nsd.unregisterServiceInfoCallback(it) }
        }
    }

    fun close() {
        for (listener in subscriptions.values) runCatching { nsd.unregisterServiceInfoCallback(listener) }
        subscriptions.clear()
    }
}

/** Coalesce address and route changes while the activity is visible. */
internal class LanChanges(context: Context, private val changed: () -> Unit) : AutoCloseable {
    private val manager = context.getSystemService(android.net.ConnectivityManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private val networks = mutableMapOf<android.net.Network, Set<String>>()
    private var registered = false
    private var initial = true
    private val notifyChange = Runnable {
        if (registered) changed()
    }
    private val callback = object : android.net.ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: android.net.Network, properties: android.net.LinkProperties) {
            val links = properties.linkAddresses.map { it.toString() }.toSet()
            handler.post {
                if (!registered) return@post
                val previous = networks.put(network, links)
                if (links != previous) schedule()
            }
        }

        override fun onLost(network: android.net.Network) {
            handler.post {
                if (registered && networks.remove(network) != null) schedule()
            }
        }
    }

    init {
        val request = android.net.NetworkRequest.Builder()
            .addTransportType(android.net.NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(android.net.NetworkCapabilities.TRANSPORT_ETHERNET)
            .build()
        registered = runCatching { manager.registerNetworkCallback(request, callback) }.isSuccess
        handler.postDelayed({ initial = false }, 800)
    }

    private fun schedule() {
        if (initial) return
        handler.removeCallbacks(notifyChange)
        handler.postDelayed(notifyChange, 500)
    }

    override fun close() {
        handler.removeCallbacks(notifyChange)
        if (registered) {
            registered = false
            manager.unregisterNetworkCallback(callback)
        }
    }
}
