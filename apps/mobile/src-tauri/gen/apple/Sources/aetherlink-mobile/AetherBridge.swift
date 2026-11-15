// Main-app side of iOS screen sharing.
//
// * `BroadcastReceiver` listens on the App Group Unix socket, receives the broadcast
//   extension's encoded frames / PCM and hands them to Rust (`aether_ios_host_*`, which
//   feeds the registered `PlatformHost`).
// * The `aether_swift_*` functions are the callbacks Rust links against (ios.rs).
// * `BroadcastPicker` triggers `RPSystemBroadcastPickerView` (the only way to start a
//   system-wide broadcast).
// * `BackgroundKeepAlive` keeps the engine process running while the user is in other apps
//   (silent mixable audio session; requires UIBackgroundModes=audio).

import AVFoundation
import Foundation
import ReplayKit
import UIKit

// MARK: - Rust → Swift

@_cdecl("aether_swift_bridge_start")
public func aetherSwiftBridgeStart() {
  BroadcastReceiver.shared.start()
  DispatchQueue.main.async {
    let name = UIDevice.current.name
    name.withCString { aether_ios_set_default_device_name($0) }
  }
}

@_cdecl("aether_swift_request_keyframe")
public func aetherSwiftRequestKeyframe() {
  BroadcastReceiver.shared.send(.requestKeyframe)
}

@_cdecl("aether_swift_viewers_changed")
public func aetherSwiftViewersChanged(_ viewers: UInt32) {
  BroadcastReceiver.shared.syncAudioWanted(force: true)
}

@_cdecl("aether_swift_show_broadcast_picker")
public func aetherSwiftShowBroadcastPicker() {
  DispatchQueue.main.async { BroadcastPicker.show() }
}

@_cdecl("aether_swift_stop_broadcast")
public func aetherSwiftStopBroadcast() {
  BroadcastReceiver.shared.send(.stopBroadcast)
}

// MARK: - Socket server

final class BroadcastReceiver {
  static let shared = BroadcastReceiver()

  private let lock = NSLock()
  private var listenFd: Int32 = -1
  private var clientFd: Int32 = -1
  private var started = false
  private var lastAudioWanted: Bool?

  func start() {
    lock.lock()
    defer { lock.unlock() }
    if started { return }
    started = true
    Thread.detachNewThread { [weak self] in self?.acceptLoop() }
    Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in self?.syncAudioWanted(force: false) }
  }

  private func acceptLoop() {
    let s = socket(AF_UNIX, SOCK_STREAM, 0)
    guard s >= 0 else { return }
    let dir = BroadcastIPC.socketDirectory().appendingPathComponent(BroadcastIPC.socketName).path
    unlink(dir)
    unlink(BroadcastIPC.socketName)
    guard BroadcastIPC.withSocketAddress({ bind(s, $0, $1) }) == 0, listen(s, 2) == 0 else {
      NSLog("AetherLink: cannot listen on broadcast socket (errno %d)", errno)
      close(s)
      return
    }
    listenFd = s
    while true {
      let c = accept(s, nil, nil)
      if c < 0 {
        if errno == EINTR { continue }
        break
      }
      var one: Int32 = 1
      setsockopt(c, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
      setClient(c)
      lastAudioWanted = nil
      syncAudioWanted(force: true)
      readLoop(c)
      setClient(-1)
      aether_ios_host_stopped()
      DispatchQueue.main.async { BackgroundKeepAlive.shared.stop() }
    }
  }

  private func setClient(_ fd: Int32) {
    lock.lock()
    if clientFd >= 0 && clientFd != fd { close(clientFd) }
    clientFd = fd
    lock.unlock()
  }

  private func readLoop(_ fd: Int32) {
    while let (type, body) = BroadcastIPC.readMessage(fd) {
      switch BroadcastMessage(rawValue: type) {
      case .config where body.count >= 11:
        aether_ios_host_config(body[body.startIndex] == 1, body.readLE(UInt32.self, at: 1), body.readLE(UInt32.self, at: 5),
                               body.readLE(UInt16.self, at: 9))
        DispatchQueue.main.async { BackgroundKeepAlive.shared.start() }
      case .display where body.count >= 12:
        aether_ios_host_display(body.readLE(UInt32.self, at: 0), body.readLE(UInt32.self, at: 4),
                                Float(bitPattern: body.readLE(UInt32.self, at: 8)), 60)
      case .codecConfig:
        body.withUnsafeBytes { aether_ios_host_codec_config($0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
      case .video where body.count > 9:
        let pts = body.readLE(UInt64.self, at: 0)
        let key = body[body.startIndex + 8] == 1
        body.withUnsafeBytes { raw in
          let p = raw.bindMemory(to: UInt8.self)
          aether_ios_host_frame(p.baseAddress?.advanced(by: 9), p.count - 9, pts, key)
        }
      case .audio where body.count > 13:
        let rate = body.readLE(UInt32.self, at: 0)
        let channels = body[body.startIndex + 4]
        let pts = body.readLE(UInt64.self, at: 5)
        let pcm = body.subdata(in: body.startIndex + 13 ..< body.endIndex)
        pcm.withUnsafeBytes { raw in
          let f = raw.bindMemory(to: Float.self)
          aether_ios_host_audio(f.baseAddress, f.count, rate, channels, pts)
        }
      case .stopped:
        aether_ios_host_stopped()
        DispatchQueue.main.async { BackgroundKeepAlive.shared.stop() }
      default:
        break
      }
    }
  }

  func send(_ type: BroadcastMessage, _ payload: Data = Data()) {
    lock.lock()
    let fd = clientFd
    lock.unlock()
    guard fd >= 0 else { return }
    _ = BroadcastIPC.writeAll(fd, BroadcastIPC.encode(type, payload))
  }

  func syncAudioWanted(force: Bool) {
    let wanted = aether_ios_audio_wanted()
    if !force && lastAudioWanted == wanted { return }
    lastAudioWanted = wanted
    send(.audioWanted, Data([wanted ? 1 : 0]))
  }
}

// MARK: - Broadcast picker

enum BroadcastPicker {
  private static var picker: RPSystemBroadcastPickerView?

  static func show() {
    guard let window = UIApplication.shared.connectedScenes
      .compactMap({ ($0 as? UIWindowScene)?.windows.first(where: { $0.isKeyWindow }) }).first else { return }
    let p = picker ?? RPSystemBroadcastPickerView(frame: CGRect(x: -100, y: -100, width: 44, height: 44))
    p.preferredExtension = BroadcastIPC.extensionBundleId
    p.showsMicrophoneButton = false
    if p.superview == nil { window.addSubview(p) }
    picker = p
    // The picker only exposes its button; tapping it programmatically opens the system sheet.
    for case let button as UIButton in p.subviews {
      button.sendActions(for: .touchUpInside)
      button.sendActions(for: .allTouchEvents)
      break
    }
  }
}

// MARK: - Background keep-alive

final class BackgroundKeepAlive {
  static let shared = BackgroundKeepAlive()
  private var engine: AVAudioEngine?

  func start() {
    guard engine == nil else { return }
    do {
      let session = AVAudioSession.sharedInstance()
      try session.setCategory(.playback, options: [.mixWithOthers])
      try session.setActive(true)
      let e = AVAudioEngine()
      let player = AVAudioPlayerNode()
      let format = AVAudioFormat(standardFormatWithSampleRate: 44_100, channels: 1)!
      e.attach(player)
      e.connect(player, to: e.mainMixerNode, format: format)
      e.mainMixerNode.outputVolume = 0
      guard let silence = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 44_100) else { return }
      silence.frameLength = silence.frameCapacity
      try e.start()
      player.scheduleBuffer(silence, at: nil, options: .loops)
      player.play()
      engine = e
    } catch {
      NSLog("AetherLink: background keep-alive unavailable: %@", "\(error)")
    }
  }

  func stop() {
    engine?.stop()
    engine = nil
    try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
  }
}
