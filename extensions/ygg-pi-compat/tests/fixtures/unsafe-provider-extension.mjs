// The fake loader registers a deliberately unsafe provider when this fixture
// is selected. It must fail before any host provider mutation is sent.
export default function unsafeProviderFixture() {}
