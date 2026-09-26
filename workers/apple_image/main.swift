// Native Apple image worker. One bounded request per process; no network model fallback.
import Foundation
import Vision
import CoreImage
import ImageIO
import UniformTypeIdentifiers
import FoundationModels

struct Point: Decodable { let x: Double; let y: Double; let include: Bool }
struct Box: Decodable { let x: Double; let y: Double; let width: Double; let height: Double }
struct Request: Decodable {
    let operation: String
    let input_path: String?
    let output_path: String?
    let points: [Point]?
    let box: Box?
    let exposure: Float?
    let noise_reduction: Float?
    let prompt: String?
}
enum Failure: String, Error {
    case invalid_request, invalid_image, pixel_limit, unsupported_raw9, assets_not_ready
    case unavailable, execution_failed, invalid_output
}
let maxPixels = 80_000_000
let context = CIContext(options: [.cacheIntermediates: false])
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!
func status(_ request: GenerateIterativeSegmentationRequest) async -> String {
    switch await request.assetStatus {
    case .ready: return "ready"
    case .notReady: return "not_ready"
    case .downloading: return "downloading"
    case .error: return "unavailable"
    @unknown default: return "unavailable"
    }
}
func inputURL(_ r: Request) throws -> URL {
    guard let path = r.input_path, path.hasPrefix("/") else { throw Failure.invalid_request }
    let url = URL(fileURLWithPath: path)
    let attrs = try FileManager.default.attributesOfItem(atPath: path)
    guard attrs[.type] as? FileAttributeType == .typeRegular,
          let size = attrs[.size] as? NSNumber, size.intValue > 0,
          size.intValue <= 100 * 1024 * 1024 else { throw Failure.invalid_request }
    return url
}
func image(_ r: Request) throws -> CGImage {
    let url = try inputURL(r)
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
          let width = props[kCGImagePropertyPixelWidth] as? Int,
          let height = props[kCGImagePropertyPixelHeight] as? Int,
          width > 0, height > 0, width <= maxPixels / height else { throw Failure.pixel_limit }
    // Display-raster geometry everywhere: apply EXIF exactly once before Vision.
    guard let ci = CIImage(contentsOf: url, options: [.applyOrientationProperty: true]),
          let cg = context.createCGImage(ci, from: ci.extent, format: .RGBA8, colorSpace: srgb)
    else { throw Failure.invalid_image }
    return cg
}
func writePNG(_ image: CIImage, _ r: Request) throws -> [String: Any] {
    guard let path = r.output_path, path.hasPrefix("/"),
          image.extent.width > 0, image.extent.height > 0,
          image.extent.width * image.extent.height <= Double(maxPixels) else { throw Failure.invalid_output }
    try context.writePNGRepresentation(of: image, to: URL(fileURLWithPath: path), format: .RGBA8,
                                       colorSpace: srgb, options: [:])
    return ["width": Int(image.extent.width), "height": Int(image.extent.height),
            "content_type": "image/png", "color_space": "srgb", "orientation": "display_pixels_top_left"]
}
func normalized(_ p: Point) throws -> NormalizedPoint {
    guard p.x.isFinite, p.y.isFinite, (0...1).contains(p.x), (0...1).contains(p.y) else { throw Failure.invalid_request }
    return NormalizedPoint(x: p.x, y: 1 - p.y)
}
func segment(_ r: Request) async throws -> [String: Any] {
    let cg = try image(r)
    let points = r.points ?? []
    guard points.count <= 16 else { throw Failure.invalid_request }
    let request: GenerateIterativeSegmentationRequest
    var first = -1
    if let box = r.box {
        guard [box.x,box.y,box.width,box.height].allSatisfy({ $0.isFinite }),
              box.x >= 0, box.y >= 0, box.width > 0, box.height > 0,
              box.x + box.width <= 1, box.y + box.height <= 1 else { throw Failure.invalid_request }
        request = GenerateIterativeSegmentationRequest(seedBox: NormalizedRect(
            x: box.x, y: 1 - box.y - box.height, width: box.width, height: box.height), .revision1)
    } else {
        guard let index = points.firstIndex(where: { $0.include }) else { throw Failure.invalid_request }
        first = index
        request = GenerateIterativeSegmentationRequest(seedPoint: try normalized(points[index]), .revision1)
    }
    // assetStatus is process-local/lazy on macOS 27; execution itself never calls downloadAssets.
    request.qualityLevel = .accurate
    for (i, point) in points.enumerated() where i != first {
        if point.include { try request.addIncludedPoint(normalized(point)) }
        else { try request.addExcludedPoint(normalized(point)) }
    }
    let handler = ImageRequestHandler(cg)
    guard let observation = try await handler.perform(request) else { throw Failure.execution_failed }
    let mask = CIImage(cgImage: try observation.cgImage)
    let resized = mask.transformed(by: CGAffineTransform(scaleX: Double(cg.width)/mask.extent.width,
                                                         y: Double(cg.height)/mask.extent.height))
    var result = try writePNG(resized, r)
    result["mask_semantics"] = "apple_vision_iterative_mask"
    result["confidence"] = observation.confidence
    result["request_revision"] = "revision1"
    return result
}
func raw(_ r: Request) throws -> [String: Any] {
    guard let filter = CIRAWFilter(imageURL: try inputURL(r)) else { throw Failure.invalid_image }
    let version: CIRAWDecoderVersion
    if filter.supportedDecoderVersions.contains(.version9) { version = .version9 }
    else if filter.supportedDecoderVersions.contains(.version9DNG) { version = .version9DNG }
    else { throw Failure.unsupported_raw9 }
    let exposure = r.exposure ?? 0
    let noise = r.noise_reduction ?? 1
    guard exposure.isFinite, (-5...5).contains(exposure), noise.isFinite, (0...1).contains(noise)
    else { throw Failure.invalid_request }
    filter.decoderVersion = version
    filter.exposure = exposure
    if filter.isLuminanceNoiseReductionSupported { filter.luminanceNoiseReductionAmount = noise }
    let size = filter.nativeSize
    guard size.width > 0, size.height > 0, size.width * size.height <= Double(maxPixels)
    else { throw Failure.pixel_limit }
    guard let output = filter.outputImage else { throw Failure.execution_failed }
    var result = try writePNG(output, r)
    result["decoder_version"] = filter.decoderVersion.rawValue
    result["render_semantics"] = "display_referred_srgb_8bit"
    return result
}
func ocr(_ r: Request) throws -> [String: Any] {
    let cg = try image(r)
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    try VNImageRequestHandler(cgImage: cg).perform([request])
    let lines: [[String: Any]] = (request.results ?? []).prefix(512).compactMap { observation in
        guard let text = observation.topCandidates(1).first else { return nil }
        let b = observation.boundingBox
        return ["text": String(text.string.prefix(2048)), "confidence": text.confidence,
                "box": ["x": b.minX, "y": 1-b.maxY, "width": b.width, "height": b.height]]
    }
    return ["width": cg.width, "height": cg.height, "lines": lines,
            "request_revision": request.revision, "coordinates": "normalized_display_top_left"]
}
func aesthetics(_ r: Request) throws -> [String: Any] {
    let cg = try image(r)
    let request = VNCalculateImageAestheticsScoresRequest()
    try VNImageRequestHandler(cgImage: cg).perform([request])
    guard let result = request.results?.first else { throw Failure.execution_failed }
    return ["overall_score": result.overallScore, "is_utility": result.isUtility,
            "request_revision": request.revision]
}
func describe(_ r: Request) async throws -> [String: Any] {
    guard SystemLanguageModel.default.isAvailable else { throw Failure.unavailable }
    let cg = try image(r)
    let prompt = r.prompt ?? "Describe this image concisely."
    guard !prompt.isEmpty, prompt.utf8.count <= 4096 else { throw Failure.invalid_request }
    let session = LanguageModelSession(model: SystemLanguageModel.default)
    let response = try await session.respond(to: Prompt { prompt; Attachment(cg) })
    guard response.content.utf8.count <= 32768 else { throw Failure.invalid_output }
    return ["text": response.content, "model": "apple_system_on_device", "cloud": false]
}
func execute(_ r: Request) async throws -> [String: Any] {
    switch r.operation {
    case "probe":
        return ["segmentation": await status(GenerateIterativeSegmentationRequest(seedPoint: .zero)),
                "foundation_models": String(describing: SystemLanguageModel.default.availability),
                "ocr": "supported", "aesthetics": "supported", "raw9": "per_input_probe_required"]
    case "probe_raw":
        guard let filter = CIRAWFilter(imageURL: try inputURL(r)) else { throw Failure.invalid_image }
        return ["supported_decoder_versions": filter.supportedDecoderVersions.map { $0.rawValue },
                "raw9_supported": filter.supportedDecoderVersions.contains(.version9) || filter.supportedDecoderVersions.contains(.version9DNG),
                "width": Int(filter.nativeSize.width), "height": Int(filter.nativeSize.height)]
    case "prepare_segmentation":
        let request = GenerateIterativeSegmentationRequest(seedPoint: .zero)
        try await request.downloadAssets()
        return ["segmentation": await status(request), "new_request_status": await status(GenerateIterativeSegmentationRequest(seedPoint: .zero))]
    case "segment": return try await segment(r)
    case "raw_render": return try raw(r)
    case "ocr": return try ocr(r)
    case "aesthetics": return try aesthetics(r)
    case "describe": return try await describe(r)
    default: throw Failure.invalid_request
    }
}
@main struct Main {
    static func main() async {
        var response: [String: Any]
        let started = Date()
        do {
            // A single read is capped; readToEnd would allocate attacker-controlled input.
            let data = FileHandle.standardInput.readData(ofLength: 65537)
            guard !data.isEmpty, data.count <= 65536 else { throw Failure.invalid_request }
            let request = try JSONDecoder().decode(Request.self, from: data)
            let result = try await execute(request)
            response = ["ok": true, "result": result]
        } catch {
            response = ["ok": false, "error": (error as? Failure)?.rawValue ?? "execution_failed",
                        "error_domain": (error as NSError).domain, "error_code": (error as NSError).code]
        }
        response["protocol"] = "infer.apple-image-worker@20260926.1"
        response["os_version"] = ProcessInfo.processInfo.operatingSystemVersionString
        response["elapsed_ms"] = Int(Date().timeIntervalSince(started) * 1000)
        // System models are OS-owned: no invented weight hash or unload claim.
        response["execution_location"] = "device"
        if let data = try? JSONSerialization.data(withJSONObject: response, options: [.sortedKeys]) {
            FileHandle.standardOutput.write(data)
            FileHandle.standardOutput.write(Data([10]))
        }
    }
}
