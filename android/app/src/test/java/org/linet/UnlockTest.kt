package org.linet

import android.app.KeyguardManager
import android.app.NotificationManager
import android.content.Context
import android.os.Looper
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.LooperMode

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29], manifest = Config.NONE)
@LooperMode(LooperMode.Mode.PAUSED)
class UnlockTest {
    private lateinit var context: Context
    private lateinit var keyguard: KeyguardManager

    @Before
    fun prepare() {
        context = RuntimeEnvironment.getApplication()
        keyguard = context.getSystemService(KeyguardManager::class.java)
        Unlock.setAuto(context, false)
    }

    private fun request(id: ULong) {
        Unlock.onRequest(context, id, "laptop")
        shadowOf(Looper.getMainLooper()).idle()
    }

    @Test
    fun lockedPhoneCannotApproveOnAndroid10() {
        shadowOf(keyguard).setIsDeviceLocked(true)
        request(1uL)
        assertEquals(false, Unlock.takeApproval(context, 1uL, true))
    }

    @Test
    fun phoneRelockedAfterRequestCannotApprove() {
        shadowOf(keyguard).setIsDeviceLocked(false)
        request(2uL)
        shadowOf(keyguard).setIsDeviceLocked(true)
        assertEquals(false, Unlock.takeApproval(context, 2uL, true))
    }

    @Test
    fun unlockedPhoneCanApproveOnlyOnce() {
        shadowOf(keyguard).setIsDeviceLocked(false)
        request(3uL)
        assertEquals(true, Unlock.takeApproval(context, 3uL, true))
        assertNull(Unlock.takeApproval(context, 3uL, true))
    }

    @Test
    fun oldAndroidApprovalOpensAuthenticationActivity() {
        shadowOf(keyguard).setIsDeviceLocked(true)
        request(4uL)
        val notification = context.getSystemService(NotificationManager::class.java).activeNotifications.single().notification
        val intent = shadowOf(notification.actions[0].actionIntent).savedIntent
        assertEquals(UnlockActivity::class.java.name, intent.component!!.className)
        assertEquals(false, Unlock.takeApproval(context, 4uL, false))
    }
}
