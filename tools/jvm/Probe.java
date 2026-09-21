// The Java half of the host-side JNI probe.
//
// `dex2rs-runtime/examples/jvm_probe.rs` is built as a cdylib and loaded here,
// so the runtime's cast rules are checked against a real JVM instead of being
// argued about. Run through `tools/host_jvm_check.sh`, which builds the library,
// compiles this file and asserts the results.
//
// Why this exists: the app died with
//
//     java.lang.ClassCastException: q5/a
//       at ...CrashActivityKt.CrashScreen$lambda$10(Native Method)
//
// because the port threw a `ClassCastException` for `(Foo) null`. In Java a cast
// of `null` always succeeds -- only a cast of a *wrongly typed non-null* value
// may throw -- and ART's `IsInstanceOf` implements exactly that, which is what
// the C++ runtime called. These probes pin all four cases down.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public final class Probe {
    private static native int checkCastNull();

    private static native int checkCastWrongType();

    private static native int checkCastSameType();

    private static native int instanceOfNull();

    private static native int exceptionDispatch();

    private static native int callResolvedMethod();

    private static native String obfuscatedString();

    private static native String obfuscatedStringSeeded();

    private static native boolean registerDynamicProbe();

    private static native int dynamicProbe(int value);

    private static native int invokeMethodHandle(MethodHandle handle, int a, int b);

    private static native int invokeVarHandle(VarHandle handle, int[] array, int index, int delta);

    private static native int lookupProbe();

    private static int failures = 0;

    /// What the `MethodHandle` probe is asked to call.
    private static int add(int a, int b) {
        return a + b;
    }

    /// `findStatic` in the probe must not see this one: a JNI frame must not
    /// silently gain private access the original dex class did not have.
    private static int secret(int a, int b) {
        return a * b;
    }

    /// An instance field the `const-method-handle` probe reads with a getter
    /// handle; `seven` is its name, `7` its value.
    private int seven = 7;

    /// For the cast probes the value is "1 if it threw", so `expected == 1`
    /// means "must throw"; for the others it is the helper's own result.
    private static void expect(String what, int actual, int expected) {
        String verdict = actual == expected ? "ok  " : "FAIL";
        if (actual != expected) {
            failures++;
        }
        System.out.printf("%s %-46s value=%d (expected %d)%n", verdict, what, actual, expected);
    }

    public static void main(String[] args) {
        String library = System.getProperty("probe.lib");
        if (library == null) {
            throw new IllegalArgumentException("pass -Dprobe.lib=<path to libjvm_probe.so>");
        }
        System.load(library);

        // A cast of null is legal for any reference type -- the device bug.
        expect("(String) null", checkCastNull(), 0);
        // A cast of a wrongly typed object must still throw.
        expect("(String) new Object()", checkCastWrongType(), 1);
        // A cast of a correctly typed object must not throw.
        expect("(String) \"x\"", checkCastSameType(), 0);
        // `instance-of` is the opposite rule: null is not an instance.
        expect("null instanceof String", instanceOfNull(), 0);

        // The landing pad's dispatch: a caught exception must match the catch
        // types its class really extends, and nothing else. 2 == RuntimeException
        // matched and Error did not.
        expect("catch dispatch (RuntimeException, not Error)", exceptionDispatch(), 2);

        // Resolve + Call: "hello".length() must come back as 5.
        expect("resolved String.length()", callResolvedMethod(), 5);

        // --obfuscate: the constant is XORed at compile time and restored just
        // before NewStringUTF. Non-ASCII, a backslash and a quote must survive.
        // Built here from code points so this file stays pure ASCII.
        String want = "h" + (char) 0x00e9 + "llo \\ \" " + new String(Character.toChars(0x1f41b));
        String got = obfuscatedString();
        if (!want.equals(got)) {
            System.out.println("     expected <" + want + "> but got <" + got + ">");
        }
        expect("obfuscated string round-trip", want.equals(got) ? 0 : 1, 0);

        // ...and the same string through the form the generator emits now: the
        // ciphertext in the probe is literally what dex2rs wrote for it.
        String seeded = obfuscatedStringSeeded();
        if (!want.equals(seeded)) {
            System.out.println("     expected <" + want + "> but got <" + seeded + ">");
        }
        expect("obfuscated string (generated literal)", want.equals(seeded) ? 0 : 1, 0);

        // --dynamic-register: the table is handed to RegisterNatives and then
        // called through, which is what every generated class does at load.
        boolean registered = registerDynamicProbe();
        expect("RegisterNatives(dynamicProbe)", registered ? 0 : 1, 0);
        if (registered) {
            expect("dynamically registered method call", dynamicProbe(20), 41);
        } else {
            System.out.println("FAIL skipping dynamicProbe(): registration failed");
            failures++;
        }

        // invoke-polymorphic: the instruction d8 emits for MethodHandle and
        // VarHandle calls. Both are driven through the runtime and must produce
        // the same values the JVM's own polymorphic call site does.
        try {
            MethodHandle add = MethodHandles.lookup().findStatic(
                    Probe.class, "add", MethodType.methodType(int.class, int.class, int.class));
            expect("invoke-polymorphic MethodHandle.invokeExact", invokeMethodHandle(add, 3, 4), 7);

            VarHandle elements = MethodHandles.arrayElementVarHandle(int[].class);
            int[] array = {10, 20, 30};
            expect("invoke-polymorphic VarHandle.getAndAdd", invokeVarHandle(elements, array, 1, 5), 20);
            // The handle has to operate on the caller's array, not a copy.
            expect("VarHandle mutation visible in Java", array[1], 25);
        } catch (Throwable error) {
            System.out.println("FAIL invoke-polymorphic probes threw: " + error);
            failures++;
        }

        // const-method-handle / const-method-type: both are resolved through
        // java.lang.invoke, so which class the VM attributes a JNI frame's
        // `MethodHandles.lookup()` to decides whether the handle works at all.
        int lookup = lookupProbe();
        if (lookup < 0) {
            System.out.println("FAIL lookupProbe stopped at step " + (-lookup));
            failures++;
        } else {
            expect("MethodHandles.lookup() from JNI sees its class", lookup & 1, 1);
            expect("lookup().findStatic(public)", lookup & 2, 2);
            expect("MethodType.methodType from a descriptor", lookup & 8, 8);
            expect("const-method-handle getter/setter", lookup & 16, 16);
            expect("const-method-handle invoke-static", lookup & 32, 32);
            if ((lookup & 4) == 0) {
                System.out.println("     note: findStatic could not reach a private method");
            }
        }

        if (failures != 0) {
            System.out.println(failures + " probe(s) failed");
            System.exit(1);
        }
        System.out.println("all probes passed");
    }
}
