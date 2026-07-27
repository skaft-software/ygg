import { render } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";

const markdownRender = vi.hoisted(() => vi.fn());

vi.mock("react-markdown", () => ({
  default: ({ children }: { children: string }) => {
    markdownRender(children);
    return <p>{children}</p>;
  },
}));

import MarkdownMessage from "./MarkdownMessage";

beforeEach(() => {
  markdownRender.mockClear();
});

it("does not parse committed Markdown again when an unrelated item updates", () => {
  const { rerender } = render(
    <MarkdownMessage content="## Stable committed response" />,
  );
  expect(markdownRender).toHaveBeenCalledTimes(1);

  rerender(<MarkdownMessage content="## Stable committed response" />);
  expect(markdownRender).toHaveBeenCalledTimes(1);

  rerender(<MarkdownMessage content="## Changed committed response" />);
  expect(markdownRender).toHaveBeenCalledTimes(2);
});
