// Parakeet Ultra (NVIDIA Parakeet TDT, moondream post-training) on the Neural Engine via FluidAudio.
// Same line protocol as the Apple engine (see main.swift): R: L: P: U: F: I: E: M:
//
// Parakeet transcribes chunks rather than streaming, so live text comes from re-transcribing the current
// utterance every ~0.6 s while you talk (cheap: ~100x real time). Always-listening ends an utterance when
// the microphone has been quiet for YH_PAUSE seconds.

import AVFoundation
import FluidAudio
import Foundation

final class Parakeet {
    let mic = Mic()
    var asr: AsrManager!
    var layers = 2
    /// Mic audio -> 16 kHz mono floats (used only on the audio thread).
    let converter = AudioConverter()
    /// The utterance being spoken, 16 kHz.
    var samples: [Float] = []
    var heardSpeech = false
    var muted = false
    var transcribing = false
    var lastPartial = Date.distantPast
    /// Bumped whenever an utterance ends or is discarded, so late partial results are ignored.
    var generation = 0
    var finishing = false

    static let rate = 16_000
    static let preroll = rate * 6 / 10 // keep 0.6 s before speech starts
    static let maxUtterance = rate * 28

    func setup() async {
        let notice = Task {
            try await Task.sleep(nanoseconds: 1_200_000_000)
            out("I:Downloading Parakeet Ultra (one time, ~0.7 GB)…")
        }
        do {
            let models = try await AsrModels.downloadAndLoad(
                version: .ultra,
                progressHandler: { p in
                    let pct = Int(p.fractionCompleted * 100)
                    if pct > 0 && pct < 100 { out("I:Downloading Parakeet Ultra… \(pct)%") }
                }
            )
            notice.cancel()
            let manager = AsrManager(config: .default)
            try await manager.loadModels(models)
            asr = manager
            layers = await manager.decoderLayerCount
        } catch {
            notice.cancel()
            fail("Couldn't load Parakeet: \(error.localizedDescription)")
        }
        if prepareOnly { out("I:ready"); exit(0) }
    }

    func transcribe(_ audio: [Float]) async -> String {
        guard audio.count > Parakeet.rate / 4, let asr else { return "" }
        var state = TdtDecoderState.make(decoderLayers: layers)
        let text = (try? await asr.transcribe(audio, decoderState: &state).text) ?? ""
        return text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Audio thread: convert and queue.
    func feed(_ buf: AVAudioPCMBuffer) {
        guard let s = try? converter.resampleBuffer(buf), !s.isEmpty else { return }
        q.async { [self] in
            if muted || finishing { return }
            samples.append(contentsOf: s)
            if Date().timeIntervalSince(mic.lastLoud) < 0.15 { heardSpeech = true }
            if !heardSpeech && samples.count > Parakeet.preroll {
                samples.removeFirst(samples.count - Parakeet.preroll)
            }
        }
    }

    func setMuted(_ on: Bool) {
        guard on != muted else { return }
        muted = on
        samples.removeAll()
        heardSpeech = false
        generation += 1
        out(on ? "M:1" : "M:0")
    }

    /// Every 0.1 s on q: live text, and (always-listening) the end of an utterance.
    func tick() {
        guard !muted, !finishing, heardSpeech else { return }
        let quiet = Date().timeIntervalSince(mic.lastLoud)
        if handsFree && (quiet >= pauseSecs || samples.count > Parakeet.maxUtterance) {
            let audio = samples
            samples.removeAll()
            heardSpeech = false
            generation += 1
            Task { [self] in
                let text = await transcribe(audio)
                q.async { if !text.isEmpty { out("U:" + text) } }
            }
            return
        }
        if !transcribing && Date().timeIntervalSince(lastPartial) > 0.6 && samples.count > Parakeet.rate / 2 {
            transcribing = true
            lastPartial = Date()
            let audio = samples
            let gen = generation
            Task { [self] in
                let text = await transcribe(audio)
                q.async { [self] in
                    transcribing = false
                    if gen == generation && !text.isEmpty && !finishing { out("P:" + text) }
                }
            }
        }
    }

    /// Push-to-talk released (stdin closed) or file finished: transcribe everything, print F:, exit.
    func finishAndExit() {
        finishing = true
        mic.stop()
        let audio = samples
        Task {
            let text = await transcribe(audio)
            if filePath != nil {
                FileHandle.standardError.write(String(format: "transcribed in %.2fs\n", Date().timeIntervalSince(started)).data(using: .utf8)!)
            }
            out("F:" + text)
            exit(0)
        }
    }

    func run() {
        Task { [self] in
            await setup()
            if let path = filePath {
                q.async { self.transcribeFile(path) }
                return
            }
            q.async { [self] in
                withMicPermission { [self] in
                    mic.start { [self] buf in feed(buf) }
                    every(0.1) { [self] in tick() }
                    whenStdinCloses({ [self] in
                        if handsFree { exit(0) } else { finishAndExit() }
                    }, onCommand: { [self] cmd in
                        if cmd == "MUTE" { setMuted(true) } else if cmd == "UNMUTE" { setMuted(false) }
                    })
                }
            }
        }
    }

    /// Testing without a microphone: --file <audio> [--hands-free plays it in real time].
    func transcribeFile(_ path: String) {
        guard let file = try? AVAudioFile(forReading: URL(fileURLWithPath: path)) else { fail("Can't open \(path)") }
        let fmt = file.processingFormat
        if handsFree {
            every(0.1) { [self] in tick() }
            whenStdinCloses({}, onCommand: { [self] cmd in
                if cmd == "MUTE" { setMuted(true) } else if cmd == "UNMUTE" { setMuted(false) }
            })
            DispatchQueue.global().async { [self] in
                while true {
                    guard let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: 1024) else { break }
                    do { try file.read(into: buf) } catch { break }
                    if buf.frameLength == 0 { break }
                    if rms(buf) * 12 > 0.08 { q.async { self.mic.lastLoud = Date() } }
                    feed(buf)
                    Thread.sleep(forTimeInterval: Double(buf.frameLength) / fmt.sampleRate)
                }
                q.asyncAfter(deadline: .now() + 2.5) { exit(0) }
            }
            return
        }
        while true {
            guard let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: 8192) else { break }
            do { try file.read(into: buf) } catch { break }
            if buf.frameLength == 0 { break }
            if let s = try? converter.resampleBuffer(buf) { samples.append(contentsOf: s) }
        }
        finishAndExit()
    }
}
