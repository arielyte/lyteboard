package app.lyteboard.android

import android.Manifest
import android.content.pm.PackageManager
import android.view.ViewOutlineProvider
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

@Composable
internal fun ScanInvitation(onResult: (String) -> Unit, onClose: () -> Unit) {
    val context = LocalContext.current
    var granted by remember {
        mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }
    var cameraError by remember { mutableStateOf(false) }
    val request = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it }
    AlertDialog(
        onDismissRequest = onClose,
        title = { Text("Scan a pairing QR") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                when {
                    cameraError -> Text("The camera is unavailable. You can paste the invitation instead.")
                    granted -> {
                        CameraPreview(onResult, { cameraError = true })
                        Text("Point the camera at the pairing code on Linux.")
                    }
                    else -> {
                        Text("Allow camera access to scan the code on Linux.")
                        Button(onClick = { request.launch(Manifest.permission.CAMERA) }) { Text("Allow camera") }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onClose) { Text("Back to invitation") } },
    )
}

@Composable
private fun CameraPreview(onResult: (String) -> Unit, onFailure: () -> Unit) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val result by rememberUpdatedState(onResult)
    val failure by rememberUpdatedState(onFailure)
    val view = remember(context) {
        PreviewView(context).apply {
            implementationMode = PreviewView.ImplementationMode.COMPATIBLE
            scaleType = PreviewView.ScaleType.FIT_CENTER
            outlineProvider = ViewOutlineProvider.BOUNDS
            clipToOutline = true
        }
    }

    DisposableEffect(owner, view) {
        val main = ContextCompat.getMainExecutor(context)
        val worker = Executors.newSingleThreadExecutor()
        val provider = ProcessCameraProvider.getInstance(context)
        val preview = Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
        val analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
        val found = AtomicBoolean(false)
        val reader = QRCodeReader()
        var disposed = false
        analysis.setAnalyzer(worker) { frame ->
            try {
                if (!found.get()) {
                    val value = decodeInvitation(frame, reader)
                    if (value != null && found.compareAndSet(false, true)) {
                        main.execute { if (!disposed) result(value) }
                    }
                }
            } finally {
                frame.close()
            }
        }
        provider.addListener({
            if (!disposed) {
                runCatching { provider.get().bindToLifecycle(owner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis) }
                    .onFailure { failure() }
            }
        }, main)
        onDispose {
            disposed = true
            analysis.clearAnalyzer()
            if (provider.isDone) runCatching { provider.get().unbind(preview, analysis) }
            worker.shutdown()
        }
    }
    AndroidView(factory = { view }, modifier = Modifier.fillMaxWidth().height(270.dp).clipToBounds())
}

private fun decodeInvitation(frame: ImageProxy, reader: QRCodeReader): String? {
    val plane = frame.planes.firstOrNull() ?: return null
    val width = frame.width
    val height = frame.height
    if (width !in 1..1920 || height !in 1..1920 || plane.pixelStride != 1) return null
    val stride = plane.rowStride
    val buffer = plane.buffer.duplicate()
    val start = buffer.position()
    if (stride < width || buffer.remaining() < (height - 1) * stride + width) return null
    val luma = ByteArray(width * height)
    for (row in 0 until height) {
        buffer.position(start + row * stride)
        buffer.get(luma, row * width, width)
    }
    return decodeInvitationQr(luma, width, height, reader)
}

internal fun decodeInvitationQr(luma: ByteArray, width: Int, height: Int, reader: QRCodeReader = QRCodeReader()): String? {
    val source = PlanarYUVLuminanceSource(luma, width, height, 0, 0, width, height, false)
    val hints = mapOf(DecodeHintType.TRY_HARDER to true)
    for (image in listOf(source, source.invert())) {
        try {
            return reader.decode(BinaryBitmap(HybridBinarizer(image)), hints).text
                ?.takeIf { it.length <= 512 && it.startsWith("lyteboard-lan:v1:") }
        } catch (_: ReaderException) {
            // Try inverted codes, which can appear in terminals using a dark palette.
        } finally {
            reader.reset()
        }
    }
    return null
}
