// AetherLink ESP32-S3 HID dongle.
// Receives frames from AetherLink (crates/aether-input/src/bt_hid/frame.rs) on the
// native USB CDC port and on UART0, and replays the HID reports as a BLE HID
// keyboard/mouse (NimBLE, HID-over-GATT) and/or a USB HID device, both using
// AetherLink's report descriptor (crates/aether-input/src/hid_report.rs).
// Build: Tools > USB Mode = "USB-OTG (TinyUSB)", USB CDC On Boot = "Enabled".
#include <Arduino.h>
#include <NimBLEDevice.h>
#include <NimBLEHIDDevice.h>
#include "USB.h"
#include "USBHID.h"

#define FW_MAJOR 0
#define FW_MINOR 1
#define FW_PATCH 0
#define PROTO_VERSION 1
#define MAX_PAYLOAD 64
#define UART_BAUD 921600  // must match one of the host's rates (921600 / 115200)

enum : uint8_t { T_HELLO = 0x01, T_INFO = 0x02, T_KEYBOARD = 0x10, T_MOUSE = 0x11, T_ABS = 0x12, T_CONSUMER = 0x13,
                 T_SET_TARGET = 0x20, T_PAIRING = 0x21, T_GET_STATUS = 0x30, T_STATUS = 0x31, T_LEDS = 0x32, T_ACK = 0x7E };
enum : uint8_t { ACK_OK = 0, ACK_UNKNOWN_TYPE = 1, ACK_BAD_LENGTH = 2, ACK_NOT_CONNECTED = 3, ACK_BAD_ARGUMENT = 4,
                 ACK_VERSION_MISMATCH = 5 };
enum : uint8_t { TARGET_USB = 1, TARGET_BLE = 2, TARGET_BOTH = 3 };
enum : uint8_t { CAP_USB = 1, CAP_BLE = 2, CAP_ABS = 4, CAP_CONSUMER = 8 };

// Byte-identical copy of aether_input::hid_report::REPORT_DESCRIPTOR.
static const uint8_t REPORT_DESCRIPTOR[] = {
  0x05,0x01,0x09,0x06,0xA1,0x01,0x85,0x01,0x05,0x07,0x19,0xE0,0x29,0xE7,0x15,0x00,0x25,0x01,0x75,0x01,0x95,0x08,
  0x81,0x02,0x95,0x01,0x75,0x08,0x81,0x01,0x95,0x05,0x75,0x01,0x05,0x08,0x19,0x01,0x29,0x05,0x91,0x02,0x95,0x01,
  0x75,0x03,0x91,0x01,0x95,0x06,0x75,0x08,0x15,0x00,0x26,0xE7,0x00,0x05,0x07,0x19,0x00,0x29,0xE7,0x81,0x00,0xC0,
  0x05,0x01,0x09,0x02,0xA1,0x01,0x85,0x02,0x09,0x01,0xA1,0x00,0x05,0x09,0x19,0x01,0x29,0x05,0x15,0x00,0x25,0x01,
  0x95,0x05,0x75,0x01,0x81,0x02,0x95,0x01,0x75,0x03,0x81,0x01,0x05,0x01,0x09,0x30,0x09,0x31,0x16,0x01,0x80,0x26,
  0xFF,0x7F,0x75,0x10,0x95,0x02,0x81,0x06,0x09,0x38,0x15,0x81,0x25,0x7F,0x75,0x08,0x95,0x01,0x81,0x06,0x05,0x0C,
  0x0A,0x38,0x02,0x15,0x81,0x25,0x7F,0x75,0x08,0x95,0x01,0x81,0x06,0xC0,0xC0,
  0x05,0x01,0x09,0x02,0xA1,0x01,0x85,0x03,0x09,0x01,0xA1,0x00,0x05,0x09,0x19,0x01,0x29,0x05,0x15,0x00,0x25,0x01,
  0x95,0x05,0x75,0x01,0x81,0x02,0x95,0x01,0x75,0x03,0x81,0x01,0x05,0x01,0x09,0x30,0x09,0x31,0x15,0x00,0x26,0xFF,
  0x7F,0x75,0x10,0x95,0x02,0x81,0x02,0xC0,0xC0,
  0x05,0x0C,0x09,0x01,0xA1,0x01,0x85,0x04,0x15,0x00,0x26,0xFF,0x03,0x19,0x00,0x2A,0xFF,0x03,0x75,0x10,0x95,0x01,
  0x81,0x00,0xC0,
};
static const uint8_t REPORT_LEN[5] = {0, 8, 7, 5, 2};  // payload length per report ID 1..4

// Receive buffer per input stream (declared before any function: Arduino's
// prototype generator inserts prototypes ahead of the first function).
struct Rx { uint8_t buf[2 * (6 + MAX_PAYLOAD + 2)]; size_t n = 0; };
static Rx rxUsb, rxUart;

// ---------------------------------------------------------------- USB HID
class DongleUsbHid : public USBHIDDevice {
 public:
  uint8_t leds = 0;
  uint16_t _onGetDescriptor(uint8_t *buf) override { memcpy(buf, REPORT_DESCRIPTOR, sizeof(REPORT_DESCRIPTOR)); return sizeof(REPORT_DESCRIPTOR); }
  void _onOutput(uint8_t id, const uint8_t *buf, uint16_t len) override { if (id == 1 && len >= 1) leds = buf[0]; }
};
static USBHID usbHid;
static DongleUsbHid usbDev;

// ---------------------------------------------------------------- BLE HID
static NimBLEServer *bleServer;
static NimBLEHIDDevice *bleHid;
static NimBLECharacteristic *bleIn[5];
static volatile bool bleConnected = false;
static bool bleAdvertisingWanted = false;
static volatile uint8_t bleLeds = 0;
static uint8_t target = TARGET_BLE;

class ServerCb : public NimBLEServerCallbacks {
  void onConnect(NimBLEServer *, NimBLEConnInfo &) override { bleConnected = true; }
  void onDisconnect(NimBLEServer *, NimBLEConnInfo &, int) override {
    bleConnected = false;
    if (bleAdvertisingWanted) NimBLEDevice::getAdvertising()->start();
  }
};
class LedCb : public NimBLECharacteristicCallbacks {
  void onWrite(NimBLECharacteristic *c, NimBLEConnInfo &) override { if (c->getLength() >= 1) bleLeds = c->getValue().data()[0]; }
};
static ServerCb serverCb;
static LedCb ledCb;

static void setupBle() {
  NimBLEDevice::init("AetherLink HID");
  NimBLEDevice::setSecurityAuth(true, false, true);  // bonding, Just Works, LE Secure Connections
  NimBLEDevice::setSecurityIOCap(BLE_HS_IO_NO_INPUT_OUTPUT);
  bleServer = NimBLEDevice::createServer();
  bleServer->setCallbacks(&serverCb);
  bleHid = new NimBLEHIDDevice(bleServer);
  bleHid->setManufacturer("AetherLink");
  bleHid->setPnp(0x02, 0x1209, 0xAE71, 0x0100);  // USB-IF VID 0x1209 (pid.codes), PID 0xAE71
  bleHid->setHidInfo(0x00, 0x03);                // country 0, RemoteWake | NormallyConnectable
  bleHid->setReportMap((uint8_t *)REPORT_DESCRIPTOR, sizeof(REPORT_DESCRIPTOR));
  for (uint8_t id = 1; id <= 4; id++) bleIn[id] = bleHid->getInputReport(id);
  bleHid->getOutputReport(1)->setCallbacks(&ledCb);
  bleHid->setBatteryLevel(100);
  bleServer->start();
  NimBLEAdvertising *adv = NimBLEDevice::getAdvertising();
  adv->setAppearance(0x03C0);  // Generic HID
  adv->addServiceUUID(bleHid->getHidService()->getUUID());
  adv->setName("AetherLink HID");
  adv->enableScanResponse(true);
  // Never discoverable by default: only let an already-bonded host reconnect.
  if (NimBLEDevice::getNumBonds() > 0) { bleAdvertisingWanted = true; adv->start(); }
}

// ---------------------------------------------------------------- framing
static uint16_t crc16(const uint8_t *d, size_t n) {  // CRC-16/CCITT-FALSE
  uint16_t crc = 0xFFFF;
  while (n--) { crc ^= (uint16_t)(*d++) << 8; for (int i = 0; i < 8; i++) crc = (crc & 0x8000) ? (crc << 1) ^ 0x1021 : crc << 1; }
  return crc;
}
static uint8_t txSeq = 0;
static void sendFrame(Stream &io, uint8_t type, const uint8_t *p, uint8_t len) {
  uint8_t f[6 + MAX_PAYLOAD + 2] = {0xA5, 0x5A, PROTO_VERSION, type, txSeq++, len};
  memcpy(f + 6, p, len);
  uint16_t crc = crc16(f + 2, 4 + len);
  f[6 + len] = crc & 0xFF; f[7 + len] = crc >> 8;
  io.write(f, 8 + len);
}
static void ack(Stream &io, uint8_t seq, uint8_t status) { uint8_t p[2] = {seq, status}; sendFrame(io, T_ACK, p, 2); }

static bool usbUp() { return (target & TARGET_USB) && usbHid.ready(); }
static bool bleUp() { return (target & TARGET_BLE) && bleConnected; }
static void statusPayload(uint8_t *p) {
  p[0] = target; p[1] = usbHid.ready(); p[2] = bleConnected;
  p[3] = NimBLEDevice::getAdvertising()->isAdvertising(); p[4] = usbDev.leds | bleLeds; p[5] = NimBLEDevice::getNumBonds();
}
static Stream *host = nullptr;  // stream that last said Hello: receives unsolicited Status

static uint8_t sendReport(uint8_t id, const uint8_t *p, uint8_t len) {
  if (len != REPORT_LEN[id]) return ACK_BAD_LENGTH;
  bool sent = false;
  if (usbUp()) sent |= usbHid.SendReport(id, p, len);
  if (bleUp()) sent |= bleIn[id]->notify(p, len);
  return sent ? ACK_OK : ACK_NOT_CONNECTED;
}

static void handleFrame(Stream &io, uint8_t type, uint8_t seq, const uint8_t *p, uint8_t len) {
  uint8_t st = ACK_OK, buf[8];
  switch (type) {
    case T_HELLO:
      if (len != 1) { st = ACK_BAD_LENGTH; break; }
      if (p[0] != PROTO_VERSION) { st = ACK_VERSION_MISMATCH; break; }
      host = &io;
      ack(io, seq, ACK_OK);
      buf[0] = PROTO_VERSION; buf[1] = FW_MAJOR; buf[2] = FW_MINOR; buf[3] = FW_PATCH;
      buf[4] = CAP_USB | CAP_BLE | CAP_ABS | CAP_CONSUMER; buf[5] = target; buf[6] = MAX_PAYLOAD; buf[7] = 0;
      sendFrame(io, T_INFO, buf, 8);
      return;
    case T_KEYBOARD: case T_MOUSE: case T_ABS: case T_CONSUMER: st = sendReport(type - T_KEYBOARD + 1, p, len); break;
    case T_SET_TARGET:
      if (len != 1) st = ACK_BAD_LENGTH; else if (p[0] < TARGET_USB || p[0] > TARGET_BOTH) st = ACK_BAD_ARGUMENT; else target = p[0];
      break;
    case T_PAIRING:
      if (len != 2) { st = ACK_BAD_LENGTH; break; }
      switch (p[0]) {
        case 0: bleAdvertisingWanted = false; NimBLEDevice::getAdvertising()->stop(); break;
        case 1: bleAdvertisingWanted = true; NimBLEDevice::getAdvertising()->start(p[1] * 1000UL); break;
        case 2: NimBLEDevice::deleteAllBonds(); break;
        case 3: for (uint16_t h : bleServer->getPeerDevices()) bleServer->disconnect(h); break;
        default: st = ACK_BAD_ARGUMENT;
      }
      break;
    case T_GET_STATUS:
      ack(io, seq, ACK_OK);
      statusPayload(buf);
      sendFrame(io, T_STATUS, buf, 6);
      return;
    default: st = ACK_UNKNOWN_TYPE;
  }
  ack(io, seq, st);
}

static void drop(Rx &r, size_t k) { memmove(r.buf, r.buf + k, r.n - k); r.n -= k; }
static void feed(Stream &io, Rx &r, uint8_t b) {
  if (r.n == sizeof(r.buf)) drop(r, 1);
  r.buf[r.n++] = b;
  for (;;) {
    size_t i = 0;  // resync: discard bytes until "A5 5A" (or a trailing A5)
    while (i < r.n && !(r.buf[i] == 0xA5 && (i + 1 == r.n || r.buf[i + 1] == 0x5A))) i++;
    if (i) drop(r, i);
    if (r.n < 6) return;
    if (r.buf[2] != PROTO_VERSION || r.buf[5] > MAX_PAYLOAD) { drop(r, 1); continue; }
    size_t total = 6 + r.buf[5] + 2;
    if (r.n < total) return;
    uint16_t crc = r.buf[total - 2] | (r.buf[total - 1] << 8);
    if (crc16(r.buf + 2, total - 4) != crc) { drop(r, 1); continue; }
    handleFrame(io, r.buf[3], r.buf[4], r.buf + 6, r.buf[5]);
    drop(r, total);
  }
}

void setup() {
  usbHid.addDevice(&usbDev, sizeof(REPORT_DESCRIPTOR));
  usbHid.begin();
  USB.productName("AetherLink HID Dongle");
  USB.manufacturerName("AetherLink");
  USB.begin();
  Serial.begin(UART_BAUD);   // native USB CDC (baud ignored)
  Serial0.begin(UART_BAUD);  // UART0 / on-board USB-UART bridge
  setupBle();
}

void loop() {
  while (Serial.available()) feed(Serial, rxUsb, Serial.read());
  while (Serial0.available()) feed(Serial0, rxUart, Serial0.read());
  static uint8_t last[6];
  static uint32_t lastMs = 0;
  if (host && millis() - lastMs > 50) {  // unsolicited Status on change
    uint8_t now[6];
    statusPayload(now);
    if (memcmp(now, last, 6) != 0) { memcpy(last, now, 6); sendFrame(*host, T_STATUS, now, 6); }
    lastMs = millis();
  }
  delay(1);
}
