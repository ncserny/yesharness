// yh-dictate: fast on-device speech-to-text for YesHarness.
//
// Engines (YH_ENGINE): "apple" (default; nothing to install) or "parakeet" (Parakeet Ultra, downloaded once).
//
// macOS 26+: Apple's SpeechAnalyzer + SpeechTranscriber (new, fast, long-form, on-device).
// Older macOS: SFSpeechRecognizer (on-device when supported).
//
// Modes:
//   (default)       push-to-talk: listen until stdin closes, then print F:<text> and exit
//   --hands-free    keep listening; after each pause print U:<utterance> and continue
//   --file <path>   transcribe an audio file (for testing), print F:<text>
//   --prepare       make sure the speech model is installed, then exit
//   --speak         read replies aloud with local voices (see Speak.swift)
//
// Input lines (stdin): MUTE / UNMUTE: ignore the microphone while the app is speaking (no echo).
//                       Closing stdin stops listening.
// Output lines: R: (mic live)  L:<0..1> (level)  P:<partial>  U:<utterance>  F:<final>
//               I:<info>  E:<error>
// Env: YH_LOCALE (e.g. de-DE), YH_PAUSE (seconds of silence that ends an utterance, default 0.9),
//      YH_VP=0 (hands-free without echo cancellation)

import AVFoundation
import Foundation
import Speech

setvbuf(stdout, nil, _IOLBF, 0)

func out(_ s: String) {
    print(s.replacingOccurrences(of: "\n", with: " "))
    fflush(stdout)
}

func fail(_ s: String) -> Never {
    out("E:" + s)
    exit(1)
}

let args = CommandLine.arguments
let env = ProcessInfo.processInfo.environment
let handsFree = args.contains("--hands-free")
let prepareOnly = args.contains("--prepare")
let filePath: String? = args.firstIndex(of: "--file").flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
let pauseSecs = Double(env["YH_PAUSE"] ?? "") ?? 0.9
let wantedLocale = env["YH_LOCALE"].map { Locale(identifier: $0) } ?? Locale.current
let started = Date()

/// All mutable state lives on this queue.
let q = DispatchQueue(label: "yh.dictate")

func rms(_ buf: AVAudioPCMBuffer) -> Float {
    guard let ch = buf.floatChannelData, buf.frameLength > 0 else { return 0 }
    let n = Int(buf.frameLength)
    var sum: Float = 0
    for i in 0..<n { sum += ch[0][i] * ch[0][i] }
    return sqrt(sum / Float(n))
}

// MARK: - microphone

final class Mic {
    let engine = AVAudioEngine()
    var lastLevelOut = Date.distantPast
    var lastLoud = Date()

    func start(_ onBuffer: @escaping (AVAudioPCMBuffer) -> Void) {
        let input = engine.inputNode
        // Always-listening: Apple's echo cancellation removes what the Mac itself plays (agents reading
        // replies aloud), so you can interrupt them by talking. YH_VP=0 turns it off.
        if handsFree && env["YH_VP"] != "0" {
            do {
                try input.setVoiceProcessingEnabled(true)
                // Don't turn other sound (the agents' voices, your music) down while listening.
                input.voiceProcessingOtherAudioDuckingConfiguration = AVAudioVoiceProcessingOtherAudioDuckingConfiguration(
                    enableAdvancedDucking: false, duckingLevel: .min)
            } catch {
                out("I:Echo cancellation unavailable: \(error.localizedDescription)")
            }
        }
        let format = input.outputFormat(forBus: 0)
        if format.sampleRate == 0 || format.channelCount == 0 { fail("No microphone found.") }
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [self] buf, _ in
            let level = min(1, rms(buf) * 12)
            q.async {
                if level > 0.08 { self.lastLoud = Date() }
                if Date().timeIntervalSince(self.lastLevelOut) > 0.07 {
                    self.lastLevelOut = Date()
                    out(String(format: "L:%.2f", level))
                }
            }
            onBuffer(buf)
        }
        engine.prepare()
        do { try engine.start() } catch { fail("Couldn't start the microphone: \(error.localizedDescription)") }
        out("R:")
    }

    func stop() {
        engine.stop()
        engine.inputNode.removeTap(onBus: 0)
    }
}

/// Repeating timer on our queue (dispatchMain() doesn't run a RunLoop, so no Foundation Timer).
var timers: [DispatchSourceTimer] = []
func every(_ secs: Double, _ f: @escaping () -> Void) {
    let t = DispatchSource.makeTimerSource(queue: q)
    t.schedule(deadline: .now() + secs, repeating: secs)
    t.setEventHandler(handler: f)
    t.resume()
    timers.append(t)
}

func whenStdinCloses(_ f: @escaping () -> Void, onCommand: @escaping (String) -> Void = { _ in }) {
    DispatchQueue.global().async {
        while let line = readLine(strippingNewline: true) {
            let cmd = line.trimmingCharacters(in: .whitespaces)
            q.async { onCommand(cmd) }
        }
        q.async(execute: f)
    }
}

func withMicPermission(_ f: @escaping () -> Void) {
    AVCaptureDevice.requestAccess(for: .audio) { granted in
        q.async {
            guard granted else {
                fail("Microphone access is off for YesHarness. Allow it in System Settings → Privacy & Security → Microphone.")
            }
            f()
        }
    }
}

// MARK: - macOS 26+: SpeechAnalyzer

@available(macOS 26, *)
final class Modern {
    var transcriber: SpeechTranscriber!
    var analyzer: SpeechAnalyzer!
    var format: AVAudioFormat!
    var converter: AVAudioConverter?
    var input: AsyncStream<AnalyzerInput>.Continuation!
    var resultsTask: Task<Void, Never>?
    var finalized = ""
    var volatile = ""
    var lastChange = Date()
    var finalizing = false
    /// The app is talking: don't listen (so its own voice never becomes your message).
    var muted = false
    let mic = Mic()

    func setMuted(_ on: Bool) {
        guard on != muted else { return }
        muted = on
        // Whatever was half-heard around the switch is dropped.
        finalized = ""
        volatile = ""
        lastChange = Date()
        out(on ? "M:1" : "M:0")
        if on {
            Task { [self] in
                try? await self.analyzer.finalize(through: nil)
                q.async { [self] in
                    finalized = ""
                    volatile = ""
                }
            }
        }
    }

    var current: String { (finalized + volatile).trimmingCharacters(in: .whitespacesAndNewlines) }

    func setup() async {
        var locale = await SpeechTranscriber.supportedLocale(equivalentTo: wantedLocale)
        if locale == nil { locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: "en-US")) }
        guard let locale else { fail("Speech recognition isn't available for your language.") }
        transcriber = SpeechTranscriber(locale: locale, transcriptionOptions: [], reportingOptions: [.volatileResults, .fastResults], attributeOptions: [])
        do {
            if let req = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
                // Usually instant (already installed); only mention it if it takes a while.
                let notice = Task {
                    try await Task.sleep(nanoseconds: 1_500_000_000)
                    out("I:Downloading Apple's speech model (one time)…")
                }
                try await req.downloadAndInstall()
                notice.cancel()
            }
        } catch {
            fail("Couldn't install the speech model: \(error.localizedDescription)")
        }
        if prepareOnly { out("I:ready"); exit(0) }
        analyzer = SpeechAnalyzer(modules: [transcriber])
        guard let f = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
            fail("No audio format available for speech recognition.")
        }
        format = f
        // Vocabulary hints: agent names, project names, file names (YH_WORDS, comma separated).
        let words = (env["YH_WORDS"] ?? "").split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        if !words.isEmpty {
            let ctx = AnalysisContext()
            ctx.contextualStrings[AnalysisContext.ContextualStringsTag("general")] = Array(words.prefix(100))
            try? await analyzer.setContext(ctx)
        }
        do { try await analyzer.prepareToAnalyze(in: format) } catch {}
        let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
        input = cont
        resultsTask = Task { [self] in
            do {
                for try await r in transcriber.results {
                    let t = String(r.text.characters)
                    q.sync {
                        if muted { return }
                        if r.isFinal { finalized += t; volatile = "" } else { volatile = t }
                        lastChange = Date()
                        if !current.isEmpty { out("P:" + current) }
                    }
                }
            } catch {
                out("I:results ended: \(error.localizedDescription)")
            }
        }
        do { try await analyzer.start(inputSequence: stream) } catch {
            fail("Couldn't start speech recognition: \(error.localizedDescription)")
        }
    }

    func feed(_ buf: AVAudioPCMBuffer) {
        if buf.format == format {
            input.yield(AnalyzerInput(buffer: buf))
            return
        }
        if converter == nil || converter!.inputFormat != buf.format {
            converter = AVAudioConverter(from: buf.format, to: format)
        }
        guard let converter else { return }
        let cap = AVAudioFrameCount(Double(buf.frameLength) * format.sampleRate / buf.format.sampleRate + 1024)
        guard let outBuf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: cap) else { return }
        var used = false
        var err: NSError?
        converter.convert(to: outBuf, error: &err) { _, status in
            if used { status.pointee = .noDataNow; return nil }
            used = true
            status.pointee = .haveData
            return buf
        }
        if err == nil && outBuf.frameLength > 0 { input.yield(AnalyzerInput(buffer: outBuf)) }
    }

    /// Stop listening, flush everything, print F: and exit.
    func finishAndExit() {
        mic.stop()
        input.finish()
        Task {
            try? await analyzer.finalizeAndFinishThroughEndOfInput()
            await resultsTask?.value
            q.sync {
                if filePath != nil {
                    FileHandle.standardError.write(String(format: "transcribed in %.2fs\n", Date().timeIntervalSince(started)).data(using: .utf8)!)
                }
                out("F:" + current)
            }
            exit(0)
        }
        // Never hang.
        q.asyncAfter(deadline: .now() + 4) { out("F:" + self.current); exit(0) }
    }

    /// Hands-free: a pause ends the utterance.
    func watchForPauses() {
        every(0.1) { [self] in
            do {
                guard !finalizing, !muted, !current.isEmpty else { return }
                let quietText = Date().timeIntervalSince(lastChange) >= pauseSecs
                let quietMic = Date().timeIntervalSince(mic.lastLoud) >= pauseSecs * 0.7
                guard quietText && quietMic else { return }
                finalizing = true
                Task { [self] in
                    try? await self.analyzer.finalize(through: nil)
                    try? await Task.sleep(nanoseconds: 120_000_000)
                    q.async { [self] in
                        let u = current
                        finalized = ""
                        volatile = ""
                        finalizing = false
                        if !u.isEmpty { out("U:" + u) }
                    }
                }
            }
        }
    }

    func run() {
        Task { [self] in
            await setup()
            if let path = filePath {
                transcribeFile(path)
                return
            }
            q.async { [self] in
                withMicPermission { [self] in
                    mic.start { [self] buf in
                        if !q.sync(execute: { muted }) { feed(buf) }
                    }
                    if handsFree {
                        q.async { self.watchForPauses() }
                    }
                    whenStdinCloses({ [self] in finishAndExit() }, onCommand: { [self] cmd in
                        if cmd == "MUTE" { setMuted(true) } else if cmd == "UNMUTE" { setMuted(false) }
                    })
                }
            }
        }
    }

    func transcribeFile(_ path: String) {
        guard let file = try? AVAudioFile(forReading: URL(fileURLWithPath: path)) else { fail("Can't open \(path)") }
        let fmt = file.processingFormat
        if handsFree {
            // Test hands-free mode: play the file in real time, as if from the mic (MUTE/UNMUTE work too).
            q.async { self.watchForPauses() }
            whenStdinCloses({}, onCommand: { [self] cmd in
                if cmd == "MUTE" { setMuted(true) } else if cmd == "UNMUTE" { setMuted(false) }
            })
            DispatchQueue.global().async { [self] in
                while true {
                    guard let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: 1024) else { break }
                    do { try file.read(into: buf) } catch { break }
                    if buf.frameLength == 0 { break }
                    if rms(buf) * 12 > 0.08 { q.async { self.mic.lastLoud = Date() } }
                    if !q.sync(execute: { muted }) { feed(buf) }
                    Thread.sleep(forTimeInterval: Double(buf.frameLength) / fmt.sampleRate)
                }
                q.asyncAfter(deadline: .now() + 2.5) { self.finishAndExit() }
            }
            return
        }
        while true {
            guard let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: 4096) else { break }
            do { try file.read(into: buf) } catch { break }
            if buf.frameLength == 0 { break }
            feed(buf)
        }
        q.async { self.finishAndExit() }
    }
}

// MARK: - older macOS: SFSpeechRecognizer

final class Legacy {
    let recognizer: SFSpeechRecognizer
    var request = SFSpeechAudioBufferRecognitionRequest()
    var task: SFSpeechRecognitionTask?
    var text = ""
    var lastChange = Date()
    var stopping = false
    var muted = false
    let mic = Mic()

    init() {
        guard let r = SFSpeechRecognizer(locale: wantedLocale) ?? SFSpeechRecognizer(locale: Locale(identifier: "en-US")) else {
            fail("Speech recognition isn't available for your language.")
        }
        recognizer = r
    }

    func newRequest() {
        request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        if recognizer.supportsOnDeviceRecognition { request.requiresOnDeviceRecognition = true }
        if #available(macOS 13, *) { request.addsPunctuation = true }
        text = ""
        task = recognizer.recognitionTask(with: request) { [self] result, error in
            q.async { [self] in
                if let r = result {
                    text = r.bestTranscription.formattedString
                    lastChange = Date()
                    if stopping && r.isFinal { out("F:" + text); exit(0) }
                    out("P:" + text)
                }
                if error != nil && stopping { out("F:" + text); exit(0) }
            }
        }
    }

    func run() {
        if prepareOnly { out("I:ready"); exit(0) }
        SFSpeechRecognizer.requestAuthorization { [self] status in
            q.async { [self] in
                guard status == .authorized else {
                    fail("Speech recognition is off for YesHarness. Allow it in System Settings → Privacy & Security → Speech Recognition.")
                }
                withMicPermission { [self] in
                    newRequest()
                    mic.start { [self] buf in
                        if !q.sync(execute: { muted }) { request.append(buf) }
                    }
                    if handsFree {
                        do {
                            every(0.1) { [self] in
                                do {
                                    let t = text.trimmingCharacters(in: .whitespaces)
                                    guard !muted, !t.isEmpty, Date().timeIntervalSince(lastChange) >= pauseSecs,
                                          Date().timeIntervalSince(mic.lastLoud) >= pauseSecs * 0.7 else { return }
                                    request.endAudio()
                                    task?.cancel()
                                    out("U:" + t)
                                    newRequest()
                                }
                            }
                        }
                    }
                    whenStdinCloses({ [self] in
                        stopping = true
                        mic.stop()
                        request.endAudio()
                        q.asyncAfter(deadline: .now() + 2) { out("F:" + self.text); exit(0) }
                    }, onCommand: { [self] cmd in
                        let on = cmd == "MUTE"
                        guard (cmd == "MUTE" || cmd == "UNMUTE"), on != muted else { return }
                        muted = on
                        out(on ? "M:1" : "M:0")
                        // Start over: nothing heard around the switch counts.
                        request.endAudio()
                        task?.cancel()
                        newRequest()
                    })
                }
            }
        }
    }
}

if args.contains("--speak") {
    // Reading replies aloud with local voices (see Speak.swift).
    let s = Speaker()
    s.run()
    withExtendedLifetime(s) { dispatchMain() }
} else if env["YH_ENGINE"] == "parakeet" {
    // NVIDIA Parakeet Ultra via FluidAudio (see Parakeet.swift).
    let p = Parakeet()
    p.mic.lastLoud = .distantPast
    p.run()
    withExtendedLifetime(p) { dispatchMain() }
} else if #available(macOS 26, *), SpeechTranscriber.isAvailable {
    let m = Modern()
    m.run()
    withExtendedLifetime(m) { dispatchMain() }
} else {
    if filePath != nil { fail("--file needs macOS 26") }
    let l = Legacy()
    l.run()
    withExtendedLifetime(l) { dispatchMain() }
}
