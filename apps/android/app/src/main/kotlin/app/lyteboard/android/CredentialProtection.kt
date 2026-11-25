package app.lyteboard.android

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.system.Os
import android.util.AtomicFile
import app.lyteboard.core.Mobile
import app.lyteboard.core.openMobile
import java.io.File
import java.io.FileNotFoundException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

internal class CredentialProtection(private val directory: File) {
    private val file = AtomicFile(File(directory, "identity-v1.sealed"))

    private external fun prepareProfile(directory: String): String?

    fun open(label: String): Mobile {
        System.loadLibrary("lyteboard_android")
        prepareProfile(directory.absolutePath)?.let { error(it) }
        return openMobile(label, LocalLink())
    }

    // These are called only by the native credential adapter.
    @Suppress("unused")
    private fun loadSeeds(): ByteArray? {
        val sealed = try {
            file.openRead().use { it.readBounded(SEALED_BYTES) }
        } catch (error: FileNotFoundException) {
            if (file.baseFile.exists()) throw error
            return null
        }
        check(sealed.size == SEALED_BYTES && sealed[0] == FORMAT)
        val key = keyStore().getKey(ALIAS, null) as? SecretKey ?: error("Identity protection key missing.")
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, sealed, 1, IV_BYTES))
        cipher.updateAAD(DOMAIN)
        return cipher.doFinal(sealed, 1 + IV_BYTES, SEALED_BYTES - 1 - IV_BYTES)
    }

    @Suppress("unused")
    private fun storeSeeds(seeds: ByteArray) {
        try {
            check(seeds.size == 64 && !file.baseFile.exists())
            val key = keyStore().getKey(ALIAS, null) as? SecretKey ?: KeyGenerator
                .getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
                    init(
                        KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                            .setKeySize(256)
                            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                            .build(),
                    )
                }.generateKey()
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, key)
            cipher.updateAAD(DOMAIN)
            val ciphertext = cipher.doFinal(seeds)
            check(cipher.iv.size == IV_BYTES && ciphertext.size == 80)
            val output = file.startWrite()
            try {
                Os.fchmod(output.fd, 0x180) // 0600
                output.write(FORMAT.toInt())
                output.write(cipher.iv)
                output.write(ciphertext)
                file.finishWrite(output)
            } catch (error: Exception) {
                file.failWrite(output)
                throw error
            }
            check(loadSeeds()?.let { decoded ->
                try {
                    java.security.MessageDigest.isEqual(decoded, seeds)
                } finally {
                    decoded.fill(0)
                }
            } == true)
        } finally {
            seeds.fill(0)
        }
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private companion object {
        const val ALIAS = "lyteboard.identity.wrap.v1"
        const val FORMAT: Byte = 1
        const val IV_BYTES = 12
        const val SEALED_BYTES = 1 + IV_BYTES + 64 + 16
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        val DOMAIN = "app.lyteboard.android.identity.v1".encodeToByteArray()
    }
}
