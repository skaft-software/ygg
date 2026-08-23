import { beforeEach, describe, expect, it, vi } from 'vitest'

const invokeMock = vi.fn()

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}))

describe('tauri-plugin-keyring-store-api', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('ping forwards payload and returns value', async () => {
    invokeMock.mockResolvedValue({ value: 'ok' })
    const api = await import('../../guest-js/index.ts')
    const out = await api.ping('test')
    expect(invokeMock).toHaveBeenCalledWith('plugin:keyring-store|ping', {
      payload: { value: 'test' },
    })
    expect(out).toBe('ok')
  })

  it('KeyringSession.load invokes initialize', async () => {
    invokeMock.mockResolvedValue(undefined)
    const api = await import('../../guest-js/index.ts')
    await api.KeyringSession.load('/snap/x', 'pwd')
    expect(invokeMock).toHaveBeenCalledWith('plugin:keyring-store|initialize', {
      snapshotPath: '/snap/x',
      password: 'pwd',
    })
  })

  it('KeyringStoreView.insert invokes save_store_record', async () => {
    invokeMock.mockResolvedValue(undefined)
    const api = await import('../../guest-js/index.ts')
    const session = await api.KeyringSession.load('/p', '')
    const client = await session.createClient('c1')
    await client.getStore().insert('k1', [1, 2, 3])
    const calls = invokeMock.mock.calls.map((c) => c[0])
    expect(calls).toContain('plugin:keyring-store|save_store_record')
    const saveCall = invokeMock.mock.calls.find((c) => c[0] === 'plugin:keyring-store|save_store_record')
    expect(saveCall?.[1]).toMatchObject({
      snapshotPath: '/p',
      key: 'k1',
      value: [1, 2, 3],
    })
  })

  it('Vault.generateSLIP10Seed invokes execute_procedure', async () => {
    invokeMock.mockResolvedValue([1, 2, 3])
    const api = await import('../../guest-js/index.ts')
    const session = await api.KeyringSession.load('/p', '')
    const client = await session.createClient('c')
    const loc = api.Location.generic('vault', 'rec')
    const vault = client.getVault('vault')
    const out = await vault.generateSLIP10Seed(loc, 32)
    expect(invokeMock).toHaveBeenCalledWith(
      'plugin:keyring-store|execute_procedure',
      expect.objectContaining({
        procedure: {
          type: 'SLIP10Generate',
          payload: expect.objectContaining({
            sizeBytes: 32,
          }),
        },
      }),
    )
    expect(out).toEqual(new Uint8Array([1, 2, 3]))
  })
})
