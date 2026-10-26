/*
 * Rufus for macOS - pipelined copy engine
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 *
 * A reader thread keeps a ring of large aligned buffers full while the calling thread
 * drains them to the destination, so source reads and USB writes overlap instead of
 * alternating. Every blocking call is a large, aligned, uncached transfer.
 */
import Foundation

enum FastIO {
    struct Failure: Error { let message: String }

    static func errnoFailure(_ what: String) -> Failure {
        Failure(message: "\(what): \(String(cString: strerror(errno)))")
    }

    private final class State {
        let lock = NSLock()
        private var _abort = false
        var abort: Bool { lock.lock(); defer { lock.unlock() }; return _abort }
        func stop() { lock.lock(); _abort = true; lock.unlock() }
    }

    /// Copies `length` bytes from `src` at `srcOffset` to the current position of `dst`.
    /// `padTo` zero-pads the final write up to a multiple of that size (raw devices need whole sectors).
    static func copy(from src: Int32, srcOffset: Int64, to dst: Int32, length: UInt64,
                     bufferSize: Int = 8 << 20, depth: Int = 4, padTo: Int = 1,
                     onBytes: (Int) -> Void, cancelled: () -> Bool) throws {
        guard length > 0 else { return }
        let slots = length > UInt64(bufferSize) ? depth : 1
        var bufs: [UnsafeMutableRawPointer] = []
        for _ in 0..<slots {
            var p: UnsafeMutableRawPointer?
            guard posix_memalign(&p, 4096, bufferSize) == 0, let b = p else {
                bufs.forEach { free($0) }
                throw Failure(message: "Out of memory")
            }
            bufs.append(b)
        }
        defer { bufs.forEach { free($0) } }

        var lens = [Int](repeating: 0, count: slots)
        var readErrno: Int32 = 0
        let freeSem = DispatchSemaphore(value: slots)
        let fullSem = DispatchSemaphore(value: 0)
        let finished = DispatchSemaphore(value: 0)
        let state = State()

        let reader = Thread {
            var off = srcOffset
            var remaining = length
            var i = 0
            while remaining > 0 {
                freeSem.wait()
                if state.abort { break }
                let want = Int(min(UInt64(bufferSize), remaining))
                var got = 0
                while got < want {
                    let n = pread(src, bufs[i] + got, want - got, off + Int64(got))
                    if n < 0 { if errno == EINTR { continue }; readErrno = errno; got = -1; break }
                    if n == 0 { readErrno = EIO; got = -1; break }      // source shorter than expected
                    got += n
                }
                lens[i] = got
                fullSem.signal()
                if got < 0 { break }
                off += Int64(got); remaining -= UInt64(got)
                i = (i + 1) % slots
            }
            finished.signal()
        }
        reader.stackSize = 1 << 20
        reader.start()

        var failure: Failure?
        var done: UInt64 = 0
        var i = 0
        while done < length {
            fullSem.wait()
            let n = lens[i]
            if n < 0 {
                errno = readErrno
                failure = errnoFailure("Read error"); break
            }
            var len = n
            if padTo > 1, len % padTo != 0 {
                let padded = (len + padTo - 1) / padTo * padTo
                memset(bufs[i] + len, 0, padded - len)
                len = padded
            }
            var off = 0
            while off < len {
                let w = write(dst, bufs[i] + off, len - off)
                if w < 0 { if errno == EINTR { continue }; failure = errnoFailure("Write error"); break }
                off += w
            }
            if failure != nil { break }
            done += UInt64(n)
            onBytes(n)
            if cancelled() { failure = Failure(message: "Cancelled by user"); break }
            freeSem.signal()
            i = (i + 1) % slots
        }
        state.stop()
        freeSem.signal()
        finished.wait()
        if let failure { throw failure }
    }
}
