import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    environment: 'node',
    include: ['tests/guest-js/**/*.test.ts'],
    clearMocks: true,
  },
})
