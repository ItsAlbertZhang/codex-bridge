import { defineConfig } from 'vitest/config'

// Run only tests/*.spec.ts. scripts/ contains manual e2e clients for real dsh processes and is excluded from vitest.
export default defineConfig({
  test: {
    include: ['tests/**/*.spec.ts'],
    environment: 'node',
    testTimeout: 10_000,
  },
})
