// Native QR scanner for iOS (AVFoundation), replacing tauri-plugin-barcode-scanner whose
// iOS entry point does not link with current Swift toolchains. Rust: `mobile_scan_qr` →
// `aether_swift_scan_qr` → result via `aether_ios_qr_result(content, error)`.

import AVFoundation
import UIKit

@_cdecl("aether_swift_scan_qr")
public func aetherSwiftScanQr() {
  DispatchQueue.main.async { QrScanner.start() }
}

enum QrScanner {
  static func finish(content: String?, error: String?) {
    func withOptionalCString(_ s: String?, _ body: (UnsafePointer<CChar>?) -> Void) {
      if let s { s.withCString { body($0) } } else { body(nil) }
    }
    withOptionalCString(content) { c in withOptionalCString(error) { e in aether_ios_qr_result(c, e) } }
  }

  static func start() {
    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized: present()
    case .notDetermined:
      AVCaptureDevice.requestAccess(for: .video) { ok in
        DispatchQueue.main.async { ok ? present() : finish(content: nil, error: "errors.cameraDenied") }
      }
    default: finish(content: nil, error: "errors.cameraDenied")
    }
  }

  private static func present() {
    let root = UIApplication.shared.connectedScenes
      .compactMap { ($0 as? UIWindowScene)?.windows.first(where: { $0.isKeyWindow })?.rootViewController }.first
    guard var top = root else { return finish(content: nil, error: "errors.unsupported") }
    while let next = top.presentedViewController { top = next }
    let vc = QrScannerViewController()
    vc.modalPresentationStyle = .fullScreen
    top.present(vc, animated: true)
  }
}

final class QrScannerViewController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
  private let session = AVCaptureSession()
  private var preview: AVCaptureVideoPreviewLayer?
  private var done = false

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .black
    guard let device = AVCaptureDevice.default(for: .video),
          let input = try? AVCaptureDeviceInput(device: device), session.canAddInput(input) else {
      complete(content: nil, error: "errors.unsupported")
      return
    }
    session.addInput(input)
    let output = AVCaptureMetadataOutput()
    if session.canAddOutput(output) {
      session.addOutput(output)
      output.setMetadataObjectsDelegate(self, queue: .main)
      output.metadataObjectTypes = [.qr]
    }
    let layer = AVCaptureVideoPreviewLayer(session: session)
    layer.videoGravity = .resizeAspectFill
    view.layer.addSublayer(layer)
    preview = layer

    let close = UIButton(type: .system)
    close.setTitle(NSLocalizedString("scanner.cancel", comment: ""), for: .normal)
    close.titleLabel?.font = .systemFont(ofSize: 17, weight: .semibold)
    close.tintColor = .white
    close.backgroundColor = UIColor(white: 0.1, alpha: 0.7)
    close.layer.cornerRadius = 22
    close.translatesAutoresizingMaskIntoConstraints = false
    close.addTarget(self, action: #selector(cancel), for: .touchUpInside)
    let hint = UILabel()
    hint.text = NSLocalizedString("scanner.hint", comment: "")
    hint.textColor = .white
    hint.textAlignment = .center
    hint.numberOfLines = 0
    hint.translatesAutoresizingMaskIntoConstraints = false
    view.addSubview(hint)
    view.addSubview(close)
    NSLayoutConstraint.activate([
      close.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -24),
      close.centerXAnchor.constraint(equalTo: view.centerXAnchor),
      close.widthAnchor.constraint(equalToConstant: 160),
      close.heightAnchor.constraint(equalToConstant: 44),
      hint.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 24),
      hint.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 24),
      hint.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -24),
    ])
    DispatchQueue.global(qos: .userInitiated).async { [session] in session.startRunning() }
  }

  override func viewDidLayoutSubviews() {
    super.viewDidLayoutSubviews()
    preview?.frame = view.bounds
  }

  func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
    guard let code = metadataObjects.compactMap({ $0 as? AVMetadataMachineReadableCodeObject }).first?.stringValue else { return }
    complete(content: code, error: nil)
  }

  @objc private func cancel() {
    complete(content: "", error: nil)
  }

  private func complete(content: String?, error: String?) {
    if done { return }
    done = true
    session.stopRunning()
    QrScanner.finish(content: content, error: error)
    dismiss(animated: true)
  }
}
