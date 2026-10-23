package io.github.snbdev.fullrecovery

import android.Manifest
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.Settings
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat
import org.json.JSONArray
import org.json.JSONObject

class MainActivity : TauriActivity() {
  private val usbManager by lazy { getSystemService(Context.USB_SERVICE) as UsbManager }

  /** Open USB connections, by device name. Kept open while the app reads them. */
  private val usbConnections = mutableMapOf<String, UsbDeviceConnection>()

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onDestroy() {
    usbConnections.values.forEach { it.close() }
    usbConnections.clear()
    super.onDestroy()
  }

  override fun onWebViewCreate(webView: WebView) {
    // Lets the interface ask for access to files and USB drives
    // (window.FullRecoveryAndroid in ui/app.js).
    webView.addJavascriptInterface(Bridge(), "FullRecoveryAndroid")
  }

  /** The USB mass storage interface (SCSI over Bulk-Only Transport), if any. */
  private fun massStorage(device: UsbDevice): UsbInterface? =
    (0 until device.interfaceCount).map { device.getInterface(it) }.firstOrNull {
      it.interfaceClass == UsbConstants.USB_CLASS_MASS_STORAGE &&
        it.interfaceSubclass == 6 &&
        it.interfaceProtocol == 0x50
    }

  inner class Bridge {
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

    /** USB sticks and card readers plugged into the phone, as JSON. */
    @JavascriptInterface
    fun usbDevices(): String {
      val list = JSONArray()
      for (device in usbManager.deviceList.values) {
        if (massStorage(device) == null) continue
        val name = listOfNotNull(device.manufacturerName, device.productName)
          .joinToString(" ").trim().ifEmpty { "USB drive" }
        list.put(
          JSONObject()
            .put("id", device.deviceName)
            .put("name", name)
            .put("permission", usbManager.hasPermission(device)),
        )
      }
      return list.toString()
    }

    /** Shows Android's "allow access to this USB device" question. */
    @JavascriptInterface
    fun requestUsbPermission(id: String) {
      val device = usbManager.deviceList[id] ?: return
      val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        PendingIntent.FLAG_MUTABLE
      } else {
        0
      }
      val intent = Intent("$packageName.USB_PERMISSION").setPackage(packageName)
      val pending = PendingIntent.getBroadcast(this@MainActivity, 0, intent, flags)
      runOnUiThread { usbManager.requestPermission(device, pending) }
    }

    /**
     * Opens a USB device for reading and returns what the Rust side needs
     * (session::UsbDevice) as JSON, or "{}" if it can't be opened.
     */
    @JavascriptInterface
    fun openUsb(id: String): String {
      val device = usbManager.deviceList[id] ?: return "{}"
      if (!usbManager.hasPermission(device)) return "{}"
      val iface = massStorage(device) ?: return "{}"
      usbConnections.remove(id)?.close()
      val connection = usbManager.openDevice(device) ?: return "{}"
      // Take the interface from Android's own storage driver.
      if (!connection.claimInterface(iface, true)) {
        connection.close()
        return "{}"
      }
      var endpointIn = -1
      var endpointOut = -1
      for (i in 0 until iface.endpointCount) {
        val endpoint = iface.getEndpoint(i)
        if (endpoint.type != UsbConstants.USB_ENDPOINT_XFER_BULK) continue
        if (endpoint.direction == UsbConstants.USB_DIR_IN) {
          endpointIn = endpoint.address
        } else {
          endpointOut = endpoint.address
        }
      }
      if (endpointIn < 0 || endpointOut < 0) {
        connection.close()
        return "{}"
      }
      usbConnections[id] = connection
      val name = listOfNotNull(device.manufacturerName, device.productName)
        .joinToString(" ").trim().ifEmpty { "USB drive" }
      return JSONObject()
        .put("name", name)
        .put("fd", connection.fileDescriptor)
        .put("interface", iface.id)
        .put("endpoint_in", endpointIn)
        .put("endpoint_out", endpointOut)
        .toString()
    }
  }
}
