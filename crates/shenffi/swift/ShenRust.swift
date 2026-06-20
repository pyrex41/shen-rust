import Foundation

/// Swift wrapper around the shenffi C ABI: a booted, embedded Shen interpreter.
///
/// The C functions (`shen_boot`, `shen_eval`, ...) are exposed to Swift via the
/// `shenffi.h` bridging header.
public final class ShenRust {
    public enum Failure: Error { case bootFailed }

    private let ctx: OpaquePointer

    /// Boots a Shen image.
    ///
    /// Pass `nil` (the default) to boot from the kernel sources embedded in the
    /// binary — no filesystem access, the recommended path on iOS. Pass a
    /// directory of `.kl` files to boot from disk instead.
    public init(kernelDir: String? = nil) throws {
        let handle: OpaquePointer? = kernelDir.map { dir in
            dir.withCString { shen_boot($0) }
        } ?? shen_boot_embedded()
        guard let handle else { throw Failure.bootFailed }
        ctx = handle
    }

    /// Evaluates a Shen expression and returns its rendered result (or an
    /// `"error: …"` string if evaluation failed).
    @discardableResult
    public func eval(_ source: String) -> String {
        guard let out = source.withCString({ shen_eval(ctx, $0) }) else { return "" }
        defer { shen_string_free(out) }
        return String(cString: out)
    }

    deinit { shen_free(ctx) }
}
