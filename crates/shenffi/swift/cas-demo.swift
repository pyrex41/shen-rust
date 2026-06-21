import Foundation

// Swift -> Rust -> (tree-shaken) shen-cas computer algebra system.
//
// The CAS reducer is deeply recursive and runs tree-walked, so drive it on a
// thread with a large stack (the shen-rust CLI does the same). In an app, call
// the FFI from a dedicated big-stack background thread.

final class CASRunner: Thread {
    override func main() {
        guard let ctx = shen_cas_boot() else {
            FileHandle.standardError.write(Data("cas boot failed\n".utf8))
            Foundation.exit(1)
        }
        defer { shen_free(ctx) }

        func cas(_ s: String) -> String {
            guard let out = s.withCString({ shen_cas_reduce(ctx, $0) }) else { return "" }
            defer { shen_string_free(out) }
            return String(cString: out)
        }

        print("--- shen-cas (embedded in Rust, called from Swift) ---")
        for expr in ["2 + 3", "6/4", "a+b*c", "Sin[x]", "D[Sin[x],x]", "D[x^3,x]", "D[Exp[x],x]"] {
            print("\(expr)  =>  \(cas(expr))")
        }
        Foundation.exit(0)
    }
}

let runner = CASRunner()
runner.stackSize = 512 * 1024 * 1024
runner.start()
while !runner.isFinished { Thread.sleep(forTimeInterval: 0.02) }
