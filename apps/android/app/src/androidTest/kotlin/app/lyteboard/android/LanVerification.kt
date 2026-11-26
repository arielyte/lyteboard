package app.lyteboard.android

import android.app.Activity
import android.app.Instrumentation
import android.content.Intent
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Bundle
import android.os.SystemClock
import android.util.Base64
import app.lyteboard.core.HistoryItem
import app.lyteboard.core.Kind
import app.lyteboard.core.Mobile
import app.lyteboard.core.PairState
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.security.MessageDigest
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import com.google.zxing.BarcodeFormat
import com.google.zxing.qrcode.QRCodeWriter

/** Command exchange for the local ADB harness. This runner is absent from the app APK. */
class LanVerification : Instrumentation() {
    override fun onCreate(arguments: Bundle?) {
        super.onCreate(arguments)
        start()
    }

    override fun onStart() {
        val result = Bundle()
        try {
            val directory = File(targetContext.filesDir, "lan6b-test").apply { mkdirs() }
            CredentialProtection(targetContext.noBackupFilesDir).open("Lyteboard Android").use { mobile ->
                mobile.setActive(true)
                val request = File(directory, "request")
                val pending = File(directory, "reply.tmp")
                val reply = File(directory, "reply")
                val deadline = SystemClock.elapsedRealtime() + 300_000
                while (SystemClock.elapsedRealtime() < deadline) {
                    if (!request.exists()) {
                        SystemClock.sleep(50)
                        continue
                    }
                    val command = JSONObject(request.readText())
                    check(request.delete())
                    val operation = command.getString("op")
                    val response = try {
                        JSONObject().put("result", perform(mobile, operation, command))
                    } catch (failure: Exception) {
                        JSONObject().put("error", failure.message ?: "Operation failed")
                    }
                    pending.writeText(response.toString())
                    check(pending.renameTo(reply))
                    if (operation == "stop") {
                        finish(Activity.RESULT_OK, result)
                        return
                    }
                }
                error("Verification harness timed out")
            }
        } catch (failure: Throwable) {
            result.putString("failure", failure.message ?: "Instrumentation failed")
            finish(Activity.RESULT_CANCELED, result)
        }
    }

    private fun perform(mobile: Mobile, operation: String, command: JSONObject): Any =
        when (operation) {
            "state" -> {
                val state = mobile.snapshot()
                val pairing = when (val pair = state.pairing) {
                    is PairState.Compare -> pair.code
                    is PairState.Failed -> "failed: ${pair.message}"
                    PairState.Complete -> "complete"
                    PairState.WaitingForPeer -> "waiting"
                    else -> ""
                }
                val devices = JSONArray()
                for (peer in state.devices) {
                    devices.put(JSONObject().put("label", peer.label).put("id", peer.id)
                        .put("connection", peer.connection.name))
                }
                JSONObject().put("id", state.id).put("pairing", pairing).put("devices", devices)
                    .put("send", state.sendStatus).put("digest", state.latest?.sha256)
                    .put("sequence", state.latest?.sequence?.toString())
            }
            "invite" -> mobile.importInvitation(command.getString("value"))
            "approve" -> mobile.confirmPairing(command.getString("value"))
            "send" -> mobile.send(command.getString("id"), Kind.valueOf(command.getString("kind")),
                Base64.decode(command.getString("bytes"), Base64.DEFAULT))
            "revoke" -> mobile.removeDevice(command.getString("id"))
            "connect" -> mobile.connect(command.getString("id"), command.getString("address"))
            "hint" -> mobile.discoveryHint(command.getString("id"), command.getString("address"))
            "network_changed" -> mobile.networkChanged()
            "history" -> mobile.setHistoryEnabled(command.getBoolean("value"))
            "history_next" -> mobile.nextHistoryItem()?.let { entry ->
                JSONObject().put("sent", entry.sent).put("device", entry.deviceId)
                    .put("digest", MessageDigest.getInstance("SHA-256").digest(entry.bytes)
                        .joinToString("") { "%02x".format(it) })
            } ?: JSONObject()
            "platform" -> verifyPlatform()
            "discovery" -> verifyDiscovery()
            "active" -> mobile.setActive(command.getBoolean("value"))
            "stop" -> Unit
            else -> error("Invalid test operation")
        }

    private fun verifyPlatform(): Boolean {
        val text = "lyteboard-lan:v1:synthetic-camera-frame"
        fun readQr(value: String): String? {
            val matrix = QRCodeWriter().encode(value, BarcodeFormat.QR_CODE, 256, 256)
            val luma = ByteArray(256 * 256) { if (matrix[it % 256, it / 256]) 0 else -1 }
            return decodeInvitationQr(luma, 256, 256)
        }
        check(readQr(text) == text)
        check(readQr("Unsupported QR") == null)
        val shared = readShared(targetContext, Intent(Intent.ACTION_SEND).setType("text/plain")
            .putExtra(Intent.EXTRA_TEXT, "Android share ✓"))
        check(shared.bytes.decodeToString() == "Android share ✓")
        val sample = ByteArray(3 * 1024 * 1024) { (it % 251).toByte() }
        val now = (System.currentTimeMillis() / 1000).toULong()
        val old = now - 8uL * 86_400uL
        val limits = Retention(2, 4, 7)
        val file = "lan6c-history-test.db"
        val path = File(targetContext.noBackupFilesDir, file)
        try {
            LocalHistory(targetContext, file).use { store ->
                check(store.list().isEmpty())
                store.add(HistoryItem("device-a", Kind.TEXT, true, old, byteArrayOf(5)), limits)
                check(store.list().isEmpty())
                store.add(HistoryItem("device-a", Kind.PNG, false, now, sample), limits)
                check(store.load(store.list().single()).bytes.contentEquals(sample))
                store.add(HistoryItem("device-b", Kind.PNG, false, now, sample), limits)
                check(store.list().single().peer == "device-b")
                store.add(HistoryItem("device-a", Kind.TEXT, true, now, shared.bytes), limits)
                store.add(HistoryItem("device-c", Kind.TEXT, true, now, shared.bytes), limits)
                check(store.list().map { it.peer } == listOf("device-c", "device-a"))
            }
            LocalHistory(targetContext, file).use { store ->
                val rows = store.list()
                check(rows.size == 2)
                check(store.load(rows[0]).bytes.contentEquals(shared.bytes))
                store.delete(rows[0].id)
                check(store.list().size == 1)
                store.clear()
                check(store.list().isEmpty())
            }
        } finally {
            android.database.sqlite.SQLiteDatabase.deleteDatabase(path)
        }
        return true
    }

    private fun verifyDiscovery(): Boolean {
        val pin = "1234".repeat(16)
        val nsd = targetContext.getSystemService(NsdManager::class.java)
        val resolved = CountDownLatch(1)
        val advertisement = NsdServiceInfo().apply {
            serviceType = "_lyteboard._tcp"
            serviceName = "lyteboard-${pin.take(16)}"
            port = 47851
            setAttribute("v", "1")
            setAttribute("id", pin)
        }
        val registration = object : NsdManager.RegistrationListener {
            override fun onRegistrationFailed(service: NsdServiceInfo, error: Int) = Unit
            override fun onUnregistrationFailed(service: NsdServiceInfo, error: Int) = Unit
            override fun onServiceRegistered(service: NsdServiceInfo) = Unit
            override fun onServiceUnregistered(service: NsdServiceInfo) = Unit
        }
        val discovery = Discovery(targetContext) { device, address ->
            if (device == pin && address.endsWith(":47851")) resolved.countDown()
        }
        try {
            runOnMainSync {
                discovery.start()
                nsd.registerService(advertisement, NsdManager.PROTOCOL_DNS_SD, registration)
            }
            return resolved.await(9, TimeUnit.SECONDS)
        } finally {
            runOnMainSync {
                discovery.close()
                runCatching { nsd.unregisterService(registration) }
            }
        }
    }

}
