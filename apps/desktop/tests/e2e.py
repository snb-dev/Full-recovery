"""End-to-end test of the real desktop app, driven through tauri-driver.

Run it with run-e2e.sh, which builds the demo SD card this test expects.
Usage: e2e.py <app binary> <screenshots> <drive name> <demo files> <phone folder>
"""
import filecmp
import os
import sys
import time

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.options import ArgOptions
from selenium.webdriver.support.ui import WebDriverWait

APP, SHOTS, DRIVE, ORIGINALS, PHONE = sys.argv[1:6]
RECOVERED = os.path.join(SHOTS, "recovered")
DELETED = [
    "DCIM/Camera/IMG_20240312_1022.jpg",
    "DCIM/Camera/IMG_20240314_1024.jpg",
    "DCIM/Camera/IMG_20240315_1025.jpg",
    "DCIM/Camera/IMG_20240317_1027.jpg",
    "DCIM/Camera/Screenshot_2024-05-02.png",
    "Documents/Project plan.docx",
    "Documents/thesis-notes.txt",
    "Videos/birthday_party.mp4",
]
os.makedirs(SHOTS, exist_ok=True)

opts = ArgOptions()
opts.set_capability("browserName", "wry")
opts.set_capability("tauri:options", {"application": APP})
d = webdriver.Remote(command_executor="http://127.0.0.1:4444", options=opts)
d.set_window_size(1280, 820)
wait = WebDriverWait(d, 30)


def shot(name):
    time.sleep(0.5)
    d.save_screenshot(os.path.join(SHOTS, f"{name}.png"))


def js(code, *args):
    return d.execute_script(code, *args)


# The list is redrawn as results arrive, so rows are read and clicked in one
# step inside the page.
def rows():
    return js("return [...document.querySelectorAll('.row-item')].map(r => r.textContent)")


def click_row(text):
    assert js("""const r = [...document.querySelectorAll('.row-item')].find(r => r.textContent.includes(arguments[0]));
                 if (r) r.click(); return !!r""", text), text


def click_category(label):
    assert js("""const li = [...document.querySelectorAll('#categories li')].find(l => l.textContent.startsWith(arguments[0]));
                 if (li) li.click(); return !!li""", label), label


try:
    # 1. Drive list.
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".drive"))
    shot("1-home")
    next(x for x in d.find_elements(By.CSS_SELECTOR, ".drive") if DRIVE in x.text).click()

    # 2. Scan choice.
    wait.until(lambda d: d.find_element(By.ID, "view-source").is_displayed())
    assert "FAT32" in d.find_element(By.ID, "partitions").text
    shot("2-source")

    # 3. Quick scan lists the deleted files with their names.
    d.find_element(By.ID, "btn-quick").click()
    wait.until(lambda d: not js("return state.scanning") and len(rows()) == len(DELETED))
    names = sorted(r.split("/")[0].replace("deleted", "", 1) for r in rows())
    assert names == sorted(os.path.basename(p) for p in DELETED), names

    click_row("IMG_20240312")
    wait.until(lambda d: js("const i = document.querySelector('#preview img'); return i && i.complete && i.naturalWidth > 0"))
    assert js("return document.querySelector('#preview img').naturalWidth") == 1600
    shot("3-results")

    # 4. Thumbnails of the deleted photos.
    click_category("Photos")
    d.find_element(By.ID, "btn-grid").click()
    wait.until(lambda d: js("const t = [...document.querySelectorAll('.tile img')]; return t.length === 5 && t.every(i => i.complete && i.naturalWidth > 0)"))
    shot("4-thumbnails")

    # 5. Text preview, including files that are not deleted.
    d.find_element(By.ID, "btn-list").click()
    click_category("All")
    d.find_element(By.ID, "chk-deleted").click()
    assert len(rows()) == 15, rows()
    click_row("thesis-notes")
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, "#preview pre"))
    assert d.find_element(By.CSS_SELECTOR, "#preview pre").text.startswith("Thesis notes")
    shot("5-text-preview")

    # 6. Video preview (needs the system's video codecs; falls back to a hex view).
    click_row("birthday_party")
    time.sleep(2)
    video = js("const v = document.querySelector('#preview video'); return v ? v.videoWidth : null")
    print("video preview:", "plays" if video else "codecs missing, hex view shown")
    shot("6-video-preview")

    # 7. Search.
    d.find_element(By.ID, "search").send_keys("IMG_2024031")
    time.sleep(0.3)
    assert len(rows()) == 8, rows()
    d.find_element(By.ID, "search").clear()
    js("document.getElementById('search').dispatchEvent(new Event('input'))")

    # 8. Deep scan: nothing new on an intact card, but it checks the content
    # of the files the quick scan found.
    d.find_element(By.ID, "btn-deep-again").click()
    wait.until(lambda d: not js("return state.scanning") and js("return state.scanKind") == "deep")
    # The results are fetched again once the scan is done.
    wait.until(lambda d: js("return state.items.some(i => i.verified)"))
    items = js("return state.items.map(i => [i.folder, i.name, i.confidence, i.verified])")
    assert not [i for i in items if i[0].startswith("/Deep scan")], items
    verified = sorted(i[1] for i in items if i[3])
    # Every photo, the video, the PDF and the DOCX; text files have no
    # structure the deep scan can check.
    expected = sorted(
        [f"IMG_2024031{n}_102{n}.jpg" for n in range(1, 9)]
        + ["Screenshot_2024-05-02.png", "Invoice_March.pdf", "Project plan.docx", "birthday_party.mp4"]
    )
    assert verified == expected, verified
    upgraded = [i[1] for i in items if i[3] and i[2] == "high"]
    assert len(upgraded) == 12, upgraded
    shot("7-after-deep-scan")

    # 9. Recover the deleted files. The folder picker is a system window that
    # can't be automated, so the command is called the way the picker would.
    js("invoke('recover', { ids: state.items.filter(i => i.deleted).map(i => i.id), out: arguments[0] })", RECOVERED)
    wait.until(lambda d: d.find_element(By.ID, "modal-title").text == "Your files are back")
    shot("8-recovered")
    for path in DELETED:
        assert filecmp.cmp(os.path.join(ORIGINALS, path), os.path.join(RECOVERED, path), shallow=False), path
    js("closeModal()")

    # 10. Phone: the same app on a phone-sized screen, looking through a
    # phone's files. The Android permission bridge is simulated.
    d.set_window_size(412, 880)
    js("""window.FullRecoveryAndroid = { hasAllFilesAccess: () => false, requestAllFilesAccess: () => {} };
          state.platform = 'android'; document.body.classList.add('platform-android');
          show('home'); checkAndroidAccess();""")
    time.sleep(0.5)
    assert d.find_element(By.ID, "android-access").is_displayed()
    shot("9-phone-permission")
    js("window.FullRecoveryAndroid.hasAllFilesAccess = () => true; checkAndroidAccess(); openFolderSource(arguments[0], 'Phone storage')", PHONE)
    wait.until(lambda d: d.find_element(By.ID, "view-source").is_displayed())
    assert not d.find_element(By.ID, "btn-deep").is_displayed()
    shot("10-phone-source")
    d.find_element(By.ID, "btn-quick").click()
    wait.until(lambda d: not js("return state.scanning") and js("return state.items.length") > 0)
    time.sleep(0.5)
    found = sorted(js("return state.items.map(i => i.found_as + ' ' + i.folder + '/' + i.name)"))
    print("phone finds:", found)
    assert found == sorted([
        "trashed /DCIM/Camera/IMG_20240312_1022.jpg",
        "trashed /DCIM/Camera/IMG_20240313_1023.jpg",
        "thumbnail /DCIM/.thumbnails/170000004.jpg",
        "thumbnail /DCIM/.thumbnails/170000005.jpg",
        "thumbnail /DCIM/.thumbnails/170000006.jpg",
        "embedded /DCIM/.thumbnails/.thumbdata4--1967290299/thumbdata4--1967290299_001.jpg",
        "embedded /DCIM/.thumbnails/.thumbdata4--1967290299/thumbdata4--1967290299_002.jpg",
        "hidden /Pictures/.private/private.jpg",
        "hidden /Android/media/com.whatsapp/WhatsApp/Media/.Statuses/status.jpg",
    ]), found
    shot("11-phone-results")
    d.find_element(By.ID, "btn-grid").click()
    wait.until(lambda d: js("const t = [...document.querySelectorAll('.tile img')]; return t.length > 0 && t.every(i => i.complete && i.naturalWidth > 0)"))
    shot("12-phone-thumbnails")
    js("document.querySelector('.tile').click()")
    wait.until(lambda d: js("const i = document.querySelector('#preview img'); return i && i.complete && i.naturalWidth > 0"))
    assert d.find_element(By.ID, "btn-close-preview").is_displayed()
    shot("13-phone-preview")
    d.find_element(By.ID, "btn-close-preview").click()
    phone_out = os.path.join(SHOTS, "phone-recovered")
    js("invoke('recover', { ids: state.items.map(i => i.id), out: arguments[0] })", phone_out)
    wait.until(lambda d: d.find_element(By.ID, "modal-title").text == "Your files are back")
    shot("14-phone-recovered")
    for name in ["IMG_20240312_1022.jpg", "IMG_20240313_1023.jpg"]:
        trashed = os.path.join(PHONE, "DCIM/Camera", f".trashed-1735689600-{name}")
        assert filecmp.cmp(trashed, os.path.join(phone_out, "DCIM/Camera", name), shallow=False), name
    js("closeModal()")

    # 11. A USB card reader plugged into the phone. Android's side is
    # simulated: there is no USB hardware here, so opening ends in a clear
    # error, which is checked too.
    js("""window.__usb = { permission: false, opened: null };
          Object.assign(window.FullRecoveryAndroid, {
            usbDevices: () => JSON.stringify([{ id: '/dev/bus/usb/001/002', name: 'Generic Card Reader',
                                                permission: window.__usb.permission }]),
            requestUsbPermission: () => setTimeout(() => { window.__usb.permission = true; }, 1500),
            openUsb: (id) => { window.__usb.opened = id;
              return JSON.stringify({ name: 'Generic Card Reader', fd: 0, interface: 0, endpoint_in: 129, endpoint_out: 2 }); },
          });
          show('home'); loadDrives();""")
    wait.until(lambda d: "Generic Card Reader" in d.find_element(By.ID, "drives").text)
    shot("15-phone-usb-drive")
    js("[...document.querySelectorAll('.drive')].find(b => b.textContent.includes('Card Reader')).click()")
    wait.until(lambda d: d.find_element(By.ID, "modal-title").text == "Allow access to the USB drive")
    shot("16-phone-usb-permission")
    wait.until(lambda d: js("return window.__usb.opened") == "/dev/bus/usb/001/002")
    wait.until(lambda d: d.find_element(By.ID, "modal-title").text == "Something went wrong")
    message = d.find_element(By.ID, "modal-body").text
    print("USB without hardware:", message)
    assert "Could not read the USB drive" in message, message
    print("PASS: all checks passed; screenshots in", SHOTS)
finally:
    d.quit()
