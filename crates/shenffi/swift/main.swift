import Foundation

// Demonstrates the Swift -> Rust -> Shen round-trip via the ShenRust wrapper.
// Usage: demo [kernel-dir]   (kernel-dir optional; nil auto-locates)

let kernelDir = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : nil

do {
    let shen = try ShenRust(kernelDir: kernelDir)
    let exprs = [
        "(+ 1 2)",
        "(* 6 7)",
        "(append [1 2 3] [4 5 6])",
        "(map (/. X (* X X)) [1 2 3 4])",
        "(+ 1 2.5)",
        "(reverse [a b c])",
        "(if (> 3 2) yes no)",
    ]
    for e in exprs {
        print("\(e)  =>  \(shen.eval(e))")
    }
} catch {
    FileHandle.standardError.write(Data("boot failed: \(error)\n".utf8))
    exit(1)
}
