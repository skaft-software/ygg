# @ygg/extension-api-v03

Schema-generated ESM runtime and TypeScript declarations for the canonical Ygg extension API 0.3 contract.

```js
import { hostOffer, selectRequired, negotiate } from '@ygg/extension-api-v03';

const offer = hostOffer(1_048_576, 4);
const contract = negotiate(offer, selectRequired(offer));
```

Generated files are regenerated from `protocol/extension-api-v0.3.schema.json`; do not edit them directly.
