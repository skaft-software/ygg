## Get chat messages

**get** `/chat/completions/{completion_id}/messages`

Get chat messages

### Path Parameters

- `completion_id: string`

### Query Parameters

- `after: optional string`

  Identifier for the last message from the previous pagination request.

- `limit: optional number`

  Number of messages to retrieve.

- `order: optional "asc" or "desc"`

  Sort order for messages by timestamp. Use `asc` for ascending order or `desc` for descending order. Defaults to `asc`.

  - `"asc"`

  - `"desc"`

### Returns

- `data: array of ChatCompletionStoreMessage`

  An array of chat completion message objects.

  - `id: string`

    The identifier of the chat message.

  - `content_parts: optional array of ChatCompletionContentPartText or ChatCompletionContentPartImage`

    If a content parts array was provided, this is an array of `text` and `image_url` parts.
    Otherwise, null.

    - `ChatCompletionContentPartText object { text, type, prompt_cache_breakpoint }`

      Learn about [text inputs](https://platform.openai.com/docs/guides/text-generation).

      - `text: string`

        The text content.

      - `type: "text"`

        The type of the content part.

        - `"text"`

      - `prompt_cache_breakpoint: optional object { mode }`

        Marks the exact end of a reusable prompt prefix. The breakpoint inherits its TTL from the request's `prompt_cache_options.ttl`; the boundary is not rounded to a token block.

        - `mode: "explicit"`

          The breakpoint mode. Always `explicit`.

          - `"explicit"`

    - `ChatCompletionContentPartImage object { image_url, type, prompt_cache_breakpoint }`

      Learn about [image inputs](https://platform.openai.com/docs/guides/vision).

      - `image_url: object { url, detail }`

        - `url: string`

          Either a URL of the image or the base64 encoded image data.

        - `detail: optional "auto" or "low" or "high"`

          Specifies the detail level of the image. Learn more in the [Vision guide](https://platform.openai.com/docs/guides/vision#low-or-high-fidelity-image-understanding).

          - `"auto"`

          - `"low"`

          - `"high"`

      - `type: "image_url"`

        The type of the content part.

        - `"image_url"`

      - `prompt_cache_breakpoint: optional object { mode }`

        Marks the exact end of a reusable prompt prefix. The breakpoint inherits its TTL from the request's `prompt_cache_options.ttl`; the boundary is not rounded to a token block.

        - `mode: "explicit"`

          The breakpoint mode. Always `explicit`.

          - `"explicit"`

- `first_id: string`

  The identifier of the first chat message in the data array.

- `has_more: boolean`

  Indicates whether there are more chat messages available.

- `last_id: string`

  The identifier of the last chat message in the data array.

- `object: "list"`

  The type of this object. It is always set to "list".

  - `"list"`

### Example

```http
curl https://api.openai.com/v1/chat/completions/$COMPLETION_ID/messages \
    -H "Authorization: Bearer $OPENAI_API_KEY"
```

#### Response

```json
{
  "data": [
    {
      "content": "content",
      "refusal": "refusal",
      "role": "assistant",
      "annotations": [
        {
          "type": "url_citation",
          "url_citation": {
            "end_index": 0,
            "start_index": 0,
            "title": "title",
            "url": "https://example.com"
          }
        }
      ],
      "audio": {
        "id": "id",
        "data": "data",
        "expires_at": 0,
        "transcript": "transcript"
      },
      "function_call": {
        "arguments": "arguments",
        "name": "name"
      },
      "tool_calls": [
        {
          "id": "id",
          "function": {
            "arguments": "arguments",
            "name": "name"
          },
          "type": "function"
        }
      ],
      "id": "id",
      "content_parts": [
        {
          "text": "text",
          "type": "text",
          "prompt_cache_breakpoint": {
            "mode": "explicit"
          }
        }
      ]
    }
  ],
  "first_id": "first_id",
  "has_more": true,
  "last_id": "last_id",
  "object": "list"
}
```

### Example

```http
curl https://api.openai.com/v1/chat/completions/chat_abc123/messages \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json"
```

#### Response

```json
{
  "object": "list",
  "data": [
    {
      "id": "chatcmpl-AyPNinnUqUDYo9SAdA52NobMflmj2-0",
      "role": "user",
      "content": "write a haiku about ai",
      "name": null,
      "content_parts": null
    }
  ],
  "first_id": "chatcmpl-AyPNinnUqUDYo9SAdA52NobMflmj2-0",
  "last_id": "chatcmpl-AyPNinnUqUDYo9SAdA52NobMflmj2-0",
  "has_more": false
}
```
