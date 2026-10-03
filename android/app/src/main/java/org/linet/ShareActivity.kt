package org.linet

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.core.content.IntentCompat

/** «Поделиться → Отправить на ноутбук». Без интерфейса: передаёт URI сервису и закрывается. */
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uris = when (intent.action) {
            Intent.ACTION_SEND ->
                listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
            Intent.ACTION_SEND_MULTIPLE ->
                IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            else -> emptyList()
        }
        // Процесс мог только что запуститься: соединение ещё устанавливается, сервис его подождёт.
        val peer = Linet.client(this).paired()
        when {
            uris.isEmpty() -> toast("Нечего отправлять")
            peer == null -> toast("Linet не сопряжён с ноутбуком")
            else -> {
                LinetService.sendFiles(this, uris)
                toast(if (uris.size == 1) "Отправка на ${peer.name}" else "Отправка ${uris.size} файлов на ${peer.name}")
            }
        }
        finish()
    }

    private fun toast(text: String) = Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
}
