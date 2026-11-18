package org.linet

import android.os.Looper
import org.robolectric.Shadows.shadowOf
import android.content.DialogInterface
import android.content.Intent
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowAlertDialog

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29], manifest = Config.NONE)
class ShareActivityTest {
    @Test
    fun externalTextRequiresConfirmationAndCanBeCancelled() {
        val intent = Intent(Intent.ACTION_SEND).putExtra(Intent.EXTRA_TEXT, "untrusted replacement")
        val controller = Robolectric.buildActivity(ShareActivity::class.java, intent).setup()
        val dialog = ShadowAlertDialog.getLatestAlertDialog()
        assertNotNull(dialog)
        assertFalse(controller.get().isFinishing)
        dialog.getButton(DialogInterface.BUTTON_NEGATIVE).performClick()
        shadowOf(Looper.getMainLooper()).idle()
        assertTrue(controller.get().isFinishing)
    }
}
