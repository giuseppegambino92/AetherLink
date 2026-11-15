// Wire format between the ReplayKit broadcast extension (AetherBroadcast) and the main app,
// over a Unix-domain stream socket in the shared App Group container.
//
//   frame := [u8 type][u32 LE length][payload]
//
// extension → app
//   1 config        [u8 hevc][u32 width][u32 height][u16 fps]
//   2 codec config  Annex-B VPS/SPS/PPS
//   3 video         [u64 pts_us][u8 keyframe] + Annex-B access unit
//   4 audio         [u32 sample_rate][u8 channels][u64 pts_us] + f32 LE interleaved samples
//   5 stopped       –
//   6 display       [u32 width][u32 height][f32 scale]
// app → extension
//   101 request keyframe
//   102 stop broadcast
//   103 audio wanted [u8 0|1]

import Foundation

enum BroadcastMessage: UInt8 {
  case config = 1
  case codecConfig = 2
  case video = 3
  case audio = 4
  case stopped = 5
  case display = 6
  case requestKeyframe = 101
  case stopBroadcast = 102
  case audioWanted = 103
}

enum BroadcastIPC {
  static let appGroup = "group.dev.aetherlink.mobile"
  static let socketName = "aetherlink-bc.sock"
  static let extensionBundleId = "dev.aetherlink.mobile.broadcast"

  /// Directory holding the socket (App Group container; falls back to the temp dir when
  /// the App Group entitlement is missing, e.g. unsigned simulator builds).
  static func socketDirectory() -> URL {
    FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
      ?? URL(fileURLWithPath: NSTemporaryDirectory())
  }

  /// `sockaddr_un.sun_path` holds 104 bytes; long container paths are reached by `chdir`
  /// + relative name instead.
  static func withSocketAddress<T>(_ body: (UnsafePointer<sockaddr>, socklen_t) -> T) -> T {
    let dir = socketDirectory().path
    var path = dir + "/" + socketName
    if path.utf8.count >= 104 {
      FileManager.default.changeCurrentDirectoryPath(dir)
      path = socketName
    }
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8.prefix(103)) + [0]
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
      raw.copyBytes(from: bytes)
    }
    addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
    return withUnsafePointer(to: &addr) {
      $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { body($0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
  }

  static func encode(_ type: BroadcastMessage, _ payload: Data = Data()) -> Data {
    var d = Data(capacity: payload.count + 5)
    d.append(type.rawValue)
    var len = UInt32(payload.count).littleEndian
    withUnsafeBytes(of: &len) { d.append(contentsOf: $0) }
    d.append(payload)
    return d
  }

  /// Blocking full write; false when the peer went away.
  static func writeAll(_ fd: Int32, _ data: Data) -> Bool {
    data.withUnsafeBytes { raw -> Bool in
      guard var p = raw.baseAddress else { return true }
      var left = raw.count
      while left > 0 {
        let n = Darwin.write(fd, p, left)
        if n <= 0 {
          if n < 0 && errno == EINTR { continue }
          return false
        }
        left -= n
        p = p.advanced(by: n)
      }
      return true
    }
  }

  /// Blocking exact read; nil on EOF / error.
  static func readExact(_ fd: Int32, _ count: Int) -> Data? {
    var out = Data(count: count)
    var got = 0
    let ok = out.withUnsafeMutableBytes { raw -> Bool in
      guard let base = raw.baseAddress else { return count == 0 }
      while got < count {
        let n = Darwin.read(fd, base.advanced(by: got), count - got)
        if n <= 0 {
          if n < 0 && errno == EINTR { continue }
          return false
        }
        got += n
      }
      return true
    }
    return ok ? out : nil
  }

  /// Reads one framed message.
  static func readMessage(_ fd: Int32) -> (UInt8, Data)? {
    guard let head = readExact(fd, 5) else { return nil }
    let type = head[head.startIndex]
    let len = head.subdata(in: head.startIndex + 1 ..< head.startIndex + 5).withUnsafeBytes { $0.loadUnaligned(as: UInt32.self) }.littleEndian
    guard len < 32 * 1024 * 1024, let body = readExact(fd, Int(len)) else { return nil }
    return (type, body)
  }
}

extension Data {
  mutating func appendLE<T: FixedWidthInteger>(_ v: T) {
    var x = v.littleEndian
    Swift.withUnsafeBytes(of: &x) { append(contentsOf: $0) }
  }

  func readLE<T: FixedWidthInteger>(_ type: T.Type, at offset: Int) -> T {
    let start = startIndex + offset
    return subdata(in: start ..< start + MemoryLayout<T>.size).withUnsafeBytes { $0.loadUnaligned(as: T.self) }.littleEndian
  }
}
