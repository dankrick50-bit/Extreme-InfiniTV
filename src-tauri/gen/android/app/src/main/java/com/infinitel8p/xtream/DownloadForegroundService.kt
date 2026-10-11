package com.infinitel8p.xtream

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import org.json.JSONObject
import java.io.File

/**
 * Keeps the process, CPU and Wi-Fi awake while the native download engine works, so downloads
 * continue with the screen off. The engine publishes a small status file; this service mirrors it
 * into a notification and stops itself once nothing is left to download.
 */
class DownloadForegroundService : Service() {

  companion object {
    private const val TAG = "DownloadKeepAlive"
    private const val CHANNEL_ID = "downloads"
    private const val NOTIFICATION_ID = 4203
    private const val LOCK_TAG = "xtream:downloads"
    private const val POLL_MS = 2000L
    private const val STALE_MS = 60_000L
    private const val IDLE_POLLS_BEFORE_STOP = 5
    private const val WAKE_LOCK_TIMEOUT_MS = 6 * 60 * 60 * 1000L

    const val ACTION_START = "com.infinitel8p.xtream.downloads.START"
    const val EXTRA_STATUS_FILE = "statusFile"
  }

  private val handler = Handler(Looper.getMainLooper())
  private var statusFile: File? = null
  private var wakeLock: PowerManager.WakeLock? = null
  private var wifiLock: WifiManager.WifiLock? = null
  private var idlePolls = 0
  private var polling = false
  private var shownText = ""
  private var shownPercent = -2

  private val poll = object : Runnable {
    override fun run() {
      if (refresh()) {
        handler.postDelayed(this, POLL_MS)
      } else {
        shutDown()
      }
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action != ACTION_START) {
      Log.w(TAG, "onStartCommand: unhandled action ${intent?.action}")
      // A started foreground service must still post its notification, or the system kills the app.
      startInForeground(buildNotification(getString(R.string.download_notification_title), "", -1))
      shutDown()
      return START_NOT_STICKY
    }
    intent.getStringExtra(EXTRA_STATUS_FILE)?.let { statusFile = File(it) }
    idlePolls = 0
    startInForeground(buildNotification(getString(R.string.download_notification_title), "", -1))
    acquireLocks()
    if (!polling) {
      polling = true
      handler.post(poll)
    }
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    handler.removeCallbacks(poll)
    polling = false
    releaseLocks()
    super.onDestroy()
  }

  // Swiping the app away ends the engine with it; the partial files stay and resume on next launch.
  override fun onTaskRemoved(rootIntent: Intent?) {
    shutDown()
    super.onTaskRemoved(rootIntent)
  }

  // Android 15+ caps this service type per day and requires it to stop when the cap is reached.
  override fun onTimeout(startId: Int, fgsType: Int) {
    shutDown()
  }

  private fun shutDown() {
    handler.removeCallbacks(poll)
    polling = false
    releaseLocks()
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  /** Returns false once the engine has nothing left to do. */
  private fun refresh(): Boolean {
    val status = readStatus()
    val unfinished = status?.optInt("unfinished", 0) ?: 0
    if (status == null || unfinished <= 0) {
      // The engine may not have written its first status yet, so a few empty reads are allowed.
      idlePolls += 1
      return idlePolls < IDLE_POLLS_BEFORE_STOP
    }
    idlePolls = 0
    acquireLocks()

    val total = status.optLong("bytesTotal", 0)
    val done = status.optLong("bytesDone", 0)
    val percent = if (total > 0) ((done * 100) / total).toInt().coerceIn(0, 100) else -1
    val title = status.optString("title", "")
    val parts = ArrayList<String>()
    if (status.optBoolean("held", false)) {
      parts.add(getString(R.string.download_notification_waiting))
    } else {
      if (title.isNotEmpty()) parts.add(title)
      if (percent >= 0) parts.add("$percent%")
    }
    if (unfinished > 1) parts.add(getString(R.string.download_notification_more, unfinished - 1))
    val text = parts.joinToString(" · ")

    if (text != shownText || percent != shownPercent) {
      shownText = text
      shownPercent = percent
      getSystemService(NotificationManager::class.java)
        .notify(NOTIFICATION_ID, buildNotification(getString(R.string.download_notification_title), text, percent))
    }
    return true
  }

  private fun readStatus(): JSONObject? {
    val file = statusFile ?: return null
    return try {
      val status = JSONObject(file.readText())
      val age = System.currentTimeMillis() - status.optLong("updatedAt", 0)
      // A file left behind by an earlier run of the app says nothing about this one.
      if (age in 0..STALE_MS) status else null
    } catch (error: Exception) {
      null
    }
  }

  private fun startInForeground(notification: Notification) {
    val manager = getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(
      NotificationChannel(
        CHANNEL_ID,
        getString(R.string.download_notification_channel),
        NotificationManager.IMPORTANCE_LOW
      )
    )
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    } else {
      startForeground(NOTIFICATION_ID, notification)
    }
  }

  private fun buildNotification(title: String, text: String, percent: Int): Notification {
    val contentIntent = PendingIntent.getActivity(
      this,
      0,
      Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
      PendingIntent.FLAG_IMMUTABLE
    )
    return Notification.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.drawable.ic_brand_mark)
      .setContentTitle(title)
      .setContentText(text)
      .setContentIntent(contentIntent)
      .setOnlyAlertOnce(true)
      .setOngoing(true)
      .setProgress(100, percent.coerceAtLeast(0), percent < 0)
      .build()
  }

  @Suppress("DEPRECATION")
  private fun acquireLocks() {
    if (wakeLock == null) {
      val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
      wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, LOCK_TAG).apply {
        setReferenceCounted(false)
      }
    }
    wakeLock?.let { if (!it.isHeld) it.acquire(WAKE_LOCK_TIMEOUT_MS) }

    if (wifiLock == null) {
      val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
      // The low-latency mode only holds with the screen on, which is the opposite of what this needs.
      wifiLock = wifiManager.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, LOCK_TAG).apply {
        setReferenceCounted(false)
      }
    }
    wifiLock?.let { if (!it.isHeld) it.acquire() }
  }

  private fun releaseLocks() {
    wakeLock?.let { if (it.isHeld) it.release() }
    wifiLock?.let { if (it.isHeld) it.release() }
  }
}
