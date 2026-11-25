package app.lyteboard.android

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper
import app.lyteboard.core.LanAccess
import app.lyteboard.core.Mobile
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
internal class Discovery(context: Context, private val mobile: Mobile) : AutoCloseable {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private val wireVersion = coreInfo().protocolVersion.toString()
    private val pending = ArrayDeque<NsdServiceInfo>()
    private var started = false
    private var resolving = false

    private val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(type: String) = Unit
        override fun onDiscoveryStopped(type: String) = Unit
        override fun onStartDiscoveryFailed(type: String, code: Int) = close()
        override fun onStopDiscoveryFailed(type: String, code: Int) = Unit
        override fun onServiceLost(service: NsdServiceInfo) = Unit
        override fun onServiceFound(service: NsdServiceInfo) {
            handler.post {
                if (started && pending.size < 16) {
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
            if (service != null && service.attributes["v"]?.decodeToString() == wireVersion) {
                val pin = service.attributes["id"]?.decodeToString()
                val ip = service.host?.hostAddress
                if (pin != null && pin.length == 64 && ip != null && service.port in 1..65535 &&
                    service.serviceName.equals("lyteboard-${pin.take(16)}", ignoreCase = true)
                ) {
                    val hint = if (ip.contains(':')) "[$ip]:${service.port}" else "$ip:${service.port}"
                    mobile.discoveryHint(pin, hint)
                }
            }
            resolveNext()
        }
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
        runCatching { nsd.stopServiceDiscovery(listener) }
    }
}
