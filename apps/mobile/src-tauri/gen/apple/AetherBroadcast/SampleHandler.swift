// ReplayKit Broadcast Upload Extension: screen (+ app audio) → VideoToolbox HEVC (H.264
// fallback) → Annex-B over the App Group Unix socket → AetherLink main app → Rust engine.
//
// Runs under the extension's ~50 MB memory limit: no frame queues, one real-time
// compression session, pixel buffers are never retained beyond the encode call.

import CoreMedia
import Foundation
import ReplayKit
import VideoToolbox

final class SampleHandler: RPBroadcastSampleHandler {
  private let ioQueue = DispatchQueue(label: "dev.aetherlink.broadcast.io")
  private var fd: Int32 = -1
  private var session: VTCompressionSession?
  private var hevc = true
  private var encodedSize = (width: Int32(0), height: Int32(0))
  private var forceKeyframe = true
  private var audioWanted = false
  private var t0: CMTime?
  private var lastVideoPts = CMTime.zero
  private let maxLongSide: Int32 = 1920
  private let fps: Int32 = 30

  // MARK: - Broadcast lifecycle

  override func broadcastStarted(withSetupInfo setupInfo: [String: NSObject]?) {
    for _ in 0 ..< 30 where fd < 0 {
      fd = connectSocket()
      if fd < 0 { usleep(100_000) }
    }
    guard fd >= 0 else {
      finishBroadcastWithError(NSError(domain: "dev.aetherlink.broadcast", code: 1, userInfo: [
        NSLocalizedDescriptionKey: NSLocalizedString("broadcast.appNotRunning", comment: ""),
      ]))
      return
    }
    let fdCopy = fd
    Thread.detachNewThread { [weak self] in self?.readLoop(fdCopy) }
  }

  override func broadcastPaused() {}

  override func broadcastResumed() {
    forceKeyframe = true
  }

  override func broadcastFinished() {
    send(.stopped)
    teardown()
  }

  private func teardown() {
    if let s = session {
      VTCompressionSessionCompleteFrames(s, untilPresentationTimeStamp: .invalid)
      VTCompressionSessionInvalidate(s)
    }
    session = nil
    ioQueue.sync {
      if fd >= 0 { close(fd) }
      fd = -1
    }
  }

  // MARK: - Socket

  private func connectSocket() -> Int32 {
    let s = socket(AF_UNIX, SOCK_STREAM, 0)
    guard s >= 0 else { return -1 }
    var one: Int32 = 1
    setsockopt(s, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
    let rc = BroadcastIPC.withSocketAddress { connect(s, $0, $1) }
    if rc != 0 {
      close(s)
      return -1
    }
    return s
  }

  private func send(_ type: BroadcastMessage, _ payload: Data = Data()) {
    let msg = BroadcastIPC.encode(type, payload)
    ioQueue.async { [weak self] in
      guard let self, self.fd >= 0 else { return }
      if !BroadcastIPC.writeAll(self.fd, msg) {
        close(self.fd)
        self.fd = -1
        self.finishBroadcastWithError(NSError(domain: "dev.aetherlink.broadcast", code: 2, userInfo: [
          NSLocalizedDescriptionKey: NSLocalizedString("broadcast.appClosed", comment: ""),
        ]))
      }
    }
  }

  private func readLoop(_ fd: Int32) {
    while let (type, body) = BroadcastIPC.readMessage(fd) {
      switch BroadcastMessage(rawValue: type) {
      case .requestKeyframe: forceKeyframe = true
      case .audioWanted: audioWanted = body.first == 1
      case .stopBroadcast:
        finishBroadcastWithError(NSError(domain: "dev.aetherlink.broadcast", code: 0, userInfo: [
          NSLocalizedDescriptionKey: NSLocalizedString("broadcast.stoppedFromApp", comment: ""),
        ]))
        return
      default: break
      }
    }
  }

  // MARK: - Samples

  override func processSampleBuffer(_ sampleBuffer: CMSampleBuffer, with sampleBufferType: RPSampleBufferType) {
    switch sampleBufferType {
    case .video: encodeVideo(sampleBuffer)
    case .audioApp: if audioWanted { forwardAudio(sampleBuffer) }
    case .audioMic: break
    @unknown default: break
    }
  }

  private func micros(_ t: CMTime) -> UInt64 {
    if t0 == nil { t0 = t }
    let d = CMTimeSubtract(t, t0!)
    return d.isValid && d.seconds > 0 ? UInt64(d.seconds * 1_000_000) : 0
  }

  // MARK: - Video

  private func encodeVideo(_ sb: CMSampleBuffer) {
    guard let pixels = CMSampleBufferGetImageBuffer(sb) else { return }
    let pts = CMSampleBufferGetPresentationTimeStamp(sb)
    // ReplayKit can exceed the target rate; keep ≤ fps.
    if lastVideoPts != .zero, CMTimeSubtract(pts, lastVideoPts).seconds < 1.0 / Double(fps) * 0.8 { return }
    lastVideoPts = pts
    let w = Int32(CVPixelBufferGetWidth(pixels))
    let h = Int32(CVPixelBufferGetHeight(pixels))
    if session == nil { createSession(sourceWidth: w, sourceHeight: h) }
    guard let session else { return }
    var props: CFDictionary?
    if forceKeyframe {
      forceKeyframe = false
      props = [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue] as CFDictionary
    }
    VTCompressionSessionEncodeFrame(session, imageBuffer: pixels, presentationTimeStamp: pts, duration: .invalid,
                                    frameProperties: props, infoFlagsOut: nil) { [weak self] status, _, sample in
      guard status == noErr, let sample, let self else { return }
      self.handleEncoded(sample)
    }
  }

  private func createSession(sourceWidth: Int32, sourceHeight: Int32) {
    let k = min(1.0, Double(maxLongSide) / Double(max(sourceWidth, sourceHeight)))
    let w = Int32(Double(sourceWidth) * k) & ~1
    let h = Int32(Double(sourceHeight) * k) & ~1
    for codec in [kCMVideoCodecType_HEVC, kCMVideoCodecType_H264] {
      var s: VTCompressionSession?
      let spec = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: kCFBooleanTrue] as CFDictionary
      let rc = VTCompressionSessionCreate(allocator: nil, width: w, height: h, codecType: codec, encoderSpecification: spec,
                                          imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil,
                                          refcon: nil, compressionSessionOut: &s)
      guard rc == noErr, let s else { continue }
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AllowFrameReordering, value: kCFBooleanFalse)
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ProfileLevel,
                           value: codec == kCMVideoCodecType_HEVC ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_High_AutoLevel)
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_MaxKeyFrameInterval, value: NSNumber(value: fps * 4))
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: NSNumber(value: fps))
      VTSessionSetProperty(s, key: kVTCompressionPropertyKey_AverageBitRate, value: NSNumber(value: Int(w) * Int(h) * Int(fps) / 12))
      VTCompressionSessionPrepareToEncodeFrames(s)
      session = s
      hevc = codec == kCMVideoCodecType_HEVC
      encodedSize = (w, h)
      var cfg = Data()
      cfg.append(hevc ? 1 : 0)
      cfg.appendLE(UInt32(w))
      cfg.appendLE(UInt32(h))
      cfg.appendLE(UInt16(fps))
      send(.config, cfg)
      var disp = Data()
      disp.appendLE(UInt32(sourceWidth))
      disp.appendLE(UInt32(sourceHeight))
      disp.appendLE(Float(1.0).bitPattern)
      send(.display, disp)
      forceKeyframe = true
      return
    }
  }

  private static let startCode: [UInt8] = [0, 0, 0, 1]

  private func handleEncoded(_ sample: CMSampleBuffer) {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
    let notSync = attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false
    let keyframe = !notSync
    if keyframe, let fmt = CMSampleBufferGetFormatDescription(sample) {
      send(.codecConfig, parameterSets(fmt))
    }
    guard let block = CMSampleBufferGetDataBuffer(sample) else { return }
    var length = 0
    var ptr: UnsafeMutablePointer<CChar>?
    guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &ptr) == noErr,
          let ptr else { return }
    var out = Data(capacity: length + 9 + 16)
    out.appendLE(micros(CMSampleBufferGetPresentationTimeStamp(sample)))
    out.append(keyframe ? 1 : 0)
    // AVCC/HVCC 4-byte big-endian lengths → Annex-B start codes.
    var offset = 0
    ptr.withMemoryRebound(to: UInt8.self, capacity: length) { bytes in
      while offset + 4 <= length {
        let n = Int(UInt32(bytes[offset]) << 24 | UInt32(bytes[offset + 1]) << 16 | UInt32(bytes[offset + 2]) << 8 | UInt32(bytes[offset + 3]))
        offset += 4
        guard n > 0, offset + n <= length else { break }
        out.append(contentsOf: SampleHandler.startCode)
        out.append(bytes.advanced(by: offset), count: n)
        offset += n
      }
    }
    send(.video, out)
  }

  private func parameterSets(_ fmt: CMFormatDescription) -> Data {
    var out = Data()
    var count = 0
    if hevc {
      CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                         parameterSetSizeOut: nil, parameterSetCountOut: &count, nalUnitHeaderLengthOut: nil)
    } else {
      CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                         parameterSetSizeOut: nil, parameterSetCountOut: &count, nalUnitHeaderLengthOut: nil)
    }
    for i in 0 ..< count {
      var p: UnsafePointer<UInt8>?
      var size = 0
      let rc = hevc
        ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: i, parameterSetPointerOut: &p,
                                                             parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil)
        : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, parameterSetIndex: i, parameterSetPointerOut: &p,
                                                             parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil)
      if rc == noErr, let p {
        out.append(contentsOf: SampleHandler.startCode)
        out.append(p, count: size)
      }
    }
    return out
  }

  // MARK: - Audio

  private func forwardAudio(_ sb: CMSampleBuffer) {
    guard let fmt = CMSampleBufferGetFormatDescription(sb),
          let asbdPtr = CMAudioFormatDescriptionGetStreamBasicDescription(fmt) else { return }
    let asbd = asbdPtr.pointee
    guard asbd.mFormatID == kAudioFormatLinearPCM, let block = CMSampleBufferGetDataBuffer(sb) else { return }
    var length = 0
    var ptr: UnsafeMutablePointer<CChar>?
    guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &ptr) == noErr,
          let ptr else { return }
    let channels = max(1, Int(asbd.mChannelsPerFrame))
    let isFloat = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0
    let bigEndian = asbd.mFormatFlags & kAudioFormatFlagIsBigEndian != 0
    let bits = Int(asbd.mBitsPerChannel)
    var out = Data()
    out.appendLE(UInt32(asbd.mSampleRate))
    out.append(UInt8(min(channels, 255)))
    out.appendLE(micros(CMSampleBufferGetPresentationTimeStamp(sb)))
    let raw = UnsafeRawPointer(ptr)
    if isFloat && bits == 32 {
      for i in 0 ..< length / 4 {
        var u = raw.loadUnaligned(fromByteOffset: i * 4, as: UInt32.self)
        u = bigEndian ? UInt32(bigEndian: u) : UInt32(littleEndian: u)
        out.appendLE(u)
      }
    } else if !isFloat && bits == 16 {
      for i in 0 ..< length / 2 {
        var u = raw.loadUnaligned(fromByteOffset: i * 2, as: UInt16.self)
        u = bigEndian ? UInt16(bigEndian: u) : UInt16(littleEndian: u)
        out.appendLE((Float(Int16(bitPattern: u)) / 32768.0).bitPattern)
      }
    } else {
      return
    }
    send(.audio, out)
  }
}
