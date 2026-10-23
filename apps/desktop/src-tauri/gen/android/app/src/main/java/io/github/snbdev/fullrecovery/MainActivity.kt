package io.github.snbdev.fullrecovery

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.Settings
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    // Lets the interface ask for access to the phone's files
    // (window.FullRecoveryAndroid in ui/app.js).
    webView.addJavascriptInterface(StorageAccess(), "FullRecoveryAndroid")
  }

  inner class StorageAccess {
    /** Whether the app may read every file of the shared storage. */
    @JavascriptInterface
    fun hasAllFilesAccess(): Boolean =
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        Environment.isExternalStorageManager()
      } else {
        ContextCompat.checkSelfPermission(
          this@MainActivity,
          Manifest.permission.READ_EXTERNAL_STORAGE,
        ) == PackageManager.PERMISSION_GRANTED
      }

    /** Opens the system screen (Android 11+) or dialog that grants access. */
    @JavascriptInterface
    fun requestAllFilesAccess() {
      runOnUiThread {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
          val intent = Intent(
            Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
            Uri.parse("package:$packageName"),
          )
          try {
            startActivity(intent)
          } catch (e: Exception) {
            startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
          }
        } else {
          requestPermissions(
            arrayOf(
              Manifest.permission.READ_EXTERNAL_STORAGE,
              Manifest.permission.WRITE_EXTERNAL_STORAGE,
            ),
          ) { }
        }
      }
    }
  }
}
