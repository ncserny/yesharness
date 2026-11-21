// Parakeet guesses the language of every sentence on its own, so a short English sentence sometimes comes
// out as Swedish ("Nu är det konstar där"). When a finished sentence is in a language you don't speak, it's
// transcribed again with Apple's recognizer in your language.
//
// Your languages: YH_LANGS ("en,de"), else the languages in System Settings.

import AVFoundation
import Foundation
import NaturalLanguage
import Speech

let userLanguages: [String] = {
    let raw = env["YH_LANGS"].map { $0.split(separator: ",").map(String.init) } ?? Locale.preferredLanguages
    var langs: [String] = []
    for l in raw {
        let code = String(l.trimmingCharacters(in: .whitespaces).lowercased().prefix(while: { $0 != "-" && $0 != "_" }))
        if !code.isEmpty && !langs.contains(code) { langs.append(code) }
    }
    return langs.isEmpty ? ["en"] : langs
}()

/// The language this text is most likely in, if it's clearly not one of yours (else nil).
func foreignLanguage(_ text: String) -> String? {
    let words = text.split(whereSeparator: { !$0.isLetter }).count
    guard words >= 3 else { return nil } // too short to tell
    let r = NLLanguageRecognizer()
    r.processString(text)
    let hyps = r.languageHypotheses(withMaximum: 6)
    let ours = hyps.filter { userLanguages.contains($0.key.rawValue) }.values.reduce(0, +)
    guard let (top, p) = hyps.max(by: { $0.value < $1.value }), !userLanguages.contains(top.rawValue) else { return nil }
    return (p > 0.6 && ours < 0.2) ? top.rawValue : nil
}

/// Apple's recognizer, in your language that fits best, on 16 kHz mono audio. nil if it can't.
func appleTranscribe(_ samples: [Float]) async -> String? {
    if SFSpeechRecognizer.authorizationStatus() == .notDetermined {
        _ = await withCheckedContinuation { (c: CheckedContinuation<SFSpeechRecognizerAuthorizationStatus, Never>) in
            SFSpeechRecognizer.requestAuthorization { c.resume(returning: $0) }
        }
    }
    guard SFSpeechRecognizer.authorizationStatus() == .authorized else { return nil }
    let supported = SFSpeechRecognizer.supportedLocales()
    let region = Locale.current.region?.identifier
    let lang = userLanguages.first ?? "en"
    let locale = supported.first(where: { $0.language.languageCode?.identifier == lang && $0.region?.identifier == region })
        ?? supported.first(where: { $0.language.languageCode?.identifier == lang })
        ?? Locale(identifier: "en-US")
    guard let rec = SFSpeechRecognizer(locale: locale), rec.isAvailable,
          let fmt = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 16_000, channels: 1, interleaved: false),
          let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: AVAudioFrameCount(samples.count)) else { return nil }
    buf.frameLength = AVAudioFrameCount(samples.count)
    samples.withUnsafeBufferPointer { src in buf.floatChannelData![0].update(from: src.baseAddress!, count: samples.count) }
    let req = SFSpeechAudioBufferRecognitionRequest()
    req.shouldReportPartialResults = false
    if rec.supportsOnDeviceRecognition { req.requiresOnDeviceRecognition = true }
    req.append(buf)
    req.endAudio()
    return await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
        var done = false
        let lock = NSLock()
        func finish(_ s: String?) {
            lock.lock(); defer { lock.unlock() }
            if done { return }
            done = true
            cont.resume(returning: s)
        }
        let task = rec.recognitionTask(with: req) { result, error in
            if let r = result, r.isFinal { finish(r.bestTranscription.formattedString) }
            else if error != nil { finish(nil) }
        }
        DispatchQueue.global().asyncAfter(deadline: .now() + 8) { task.cancel(); finish(nil) }
    }
}

/// Parakeet's text, or Apple's if Parakeet heard a language you don't speak.
func inYourLanguage(_ text: String, audio: [Float]) async -> String {
    guard let foreign = foreignLanguage(text) else { return text }
    let redo = await appleTranscribe(audio)
    if env["YH_DEBUG"] != nil {
        FileHandle.standardError.write("language guard: heard \(foreign) (you speak \(userLanguages)); speech permission \(SFSpeechRecognizer.authorizationStatus().rawValue); Apple: \(redo ?? "nil")\n".data(using: .utf8)!)
    }
    guard let redo, !redo.isEmpty else { return text }
    return redo
}
