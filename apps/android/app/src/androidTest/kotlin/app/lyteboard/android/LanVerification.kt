package app.lyteboard.android

import android.app.Activity
import android.app.Instrumentation
import android.os.Bundle
import android.os.SystemClock
import android.util.Base64
import app.lyteboard.core.Kind
import app.lyteboard.core.Mobile
import app.lyteboard.core.PairState
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

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
            "active" -> mobile.setActive(command.getBoolean("value"))
            "stop" -> Unit
            else -> error("Invalid test operation")
        }
}
