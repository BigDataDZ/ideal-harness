declare module "node:assert/strict" {
  interface StrictAssert {
    equal(actual: unknown, expected: unknown): void;
    deepEqual(actual: unknown, expected: unknown): void;
    ok(value: unknown): void;
    throws(block: () => unknown): void;
  }

  const assert: StrictAssert;
  export default assert;
}

declare module "node:test" {
  function test(name: string, block: () => void | Promise<void>): void;
  export default test;
}
