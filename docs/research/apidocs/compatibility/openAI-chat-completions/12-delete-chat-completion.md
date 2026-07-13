## Delete chat completion

**delete** `/chat/completions/{completion_id}`

Delete chat completion

### Path Parameters

- `completion_id: string`

### Returns

- `ChatCompletionDeleted object { id, deleted, object }`

  - `id: string`

    The ID of the chat completion that was deleted.

  - `deleted: boolean`

    Whether the chat completion was deleted.

  - `object: "chat.completion.deleted"`

    The type of object being deleted.

    - `"chat.completion.deleted"`

### Example

```http
curl https://api.openai.com/v1/chat/completions/$COMPLETION_ID \
    -X DELETE \
    -H "Authorization: Bearer $OPENAI_API_KEY"
```

#### Response

```json
{
  "id": "id",
  "deleted": true,
  "object": "chat.completion.deleted"
}
```

### Example

```http
curl -X DELETE https://api.openai.com/v1/chat/completions/chat_abc123 \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json"
```

#### Response

```json
{
  "object": "chat.completion.deleted",
  "id": "chatcmpl-AyPNinnUqUDYo9SAdA52NobMflmj2",
  "deleted": true
}
```
