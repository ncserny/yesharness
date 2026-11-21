// Local voices for reading replies aloud, on this Mac (no account, no cloud):
//   English: Kokoro-82M on the Neural Engine (Apache-2.0)
//   German, French, Italian, Portuguese: Kyutai Pocket TTS
// Mode --speak: one request per stdin line, JSON {"text", "voice", "speed", "out"}; the WAV goes to "out".
// Output lines: I:<loading…>  R: (ready)  D:<out path>  E:<out path>\t<error>  ("unsupported" = use another voice)

import FluidAudio
import Foundation
import NaturalLanguage

final class Speaker: @unchecked Sendable {
    var kokoro: KokoroAneManager?
    var pocket: [String: PocketTtsManager] = [:]

    static let pocketLanguages: [String: PocketTtsLanguage] = ["de": .german, "fr": .french24L, "it": .italian, "pt": .portuguese]

    func language(of text: String) -> String {
        let r = NLLanguageRecognizer()
        r.languageConstraints = [.english, .german, .french, .italian, .portuguese, .spanish]
        r.processString(text)
        return r.dominantLanguage?.rawValue ?? "en"
    }

    func kokoroManager() async throws -> KokoroAneManager {
        if let k = kokoro { return k }
        out("I:Loading the local voice…")
        let k = KokoroAneManager(variant: .english)
        try await k.initialize()
        kokoro = k
        return k
    }

    func pocketManager(_ lang: String) async throws -> PocketTtsManager? {
        if let p = pocket[lang] { return p }
        guard let l = Speaker.pocketLanguages[lang] else { return nil }
        out("I:Loading the local voice…")
        let p = PocketTtsManager(language: l)
        try await p.initialize()
        pocket[lang] = p
        return p
    }

    func handle(_ line: String) async {
        guard let data = line.data(using: .utf8),
              let req = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let text = req["text"] as? String, let outPath = req["out"] as? String else {
            out("E:\tbad request")
            return
        }
        let speed = Float((req["speed"] as? Double) ?? 1.0)
        let voice = req["voice"] as? String
        do {
            let lang = language(of: text)
            let wav: Data
            if lang == "en" {
                let k = try await kokoroManager()
                let v = voice.flatMap { KokoroAneConstants.englishVoices.contains($0) ? $0 : nil }
                wav = try await k.synthesize(text: text, voice: v, speed: speed)
            } else if let p = try await pocketManager(lang) {
                wav = try await p.synthesize(text: text)
            } else {
                out("E:" + outPath + "\tunsupported")
                return
            }
            try wav.write(to: URL(fileURLWithPath: outPath))
            out("D:" + outPath)
        } catch {
            out("E:" + outPath + "\t" + error.localizedDescription)
        }
    }

    func run() {
        let lines = AsyncStream<String> { cont in
            DispatchQueue.global().async {
                while let l = readLine() { cont.yield(l) }
                cont.finish()
            }
        }
        Task {
            // Warm up English so the first reply doesn't wait for the model.
            _ = try? await kokoroManager()
            out("R:")
            for await l in lines { await handle(l) }
            exit(0)
        }
    }
}
