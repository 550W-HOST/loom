// @vitest-environment jsdom

import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { HashNavigationScroll } from "./App";

describe("HashNavigationScroll", () => {
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("scrolls to a destination that is already mounted", async () => {
    const scrollIntoView = vi.spyOn(Element.prototype, "scrollIntoView");
    const focus = vi.spyOn(HTMLElement.prototype, "focus");

    render(
      <MemoryRouter initialEntries={["/automations#configuration"]}>
        <HashNavigationScroll />
        <div id="configuration" />
      </MemoryRouter>,
    );

    await waitFor(() => {
      expect(scrollIntoView).toHaveBeenCalledWith({
        block: "start",
        inline: "nearest",
      });
      expect(focus).toHaveBeenCalledWith({ preventScroll: true });
    });
  });

  it("waits for lazy product surfaces to mount", async () => {
    const scrollIntoView = vi.spyOn(Element.prototype, "scrollIntoView");
    const view = render(
      <MemoryRouter initialEntries={["/#lazy-product-content"]}>
        <HashNavigationScroll />
      </MemoryRouter>,
    );

    expect(scrollIntoView).not.toHaveBeenCalled();

    view.rerender(
      <MemoryRouter initialEntries={["/#lazy-product-content"]}>
        <HashNavigationScroll />
        <section id="lazy-product-content" />
      </MemoryRouter>,
    );

    await waitFor(() => {
      expect(scrollIntoView).toHaveBeenCalledWith({
        block: "start",
        inline: "nearest",
      });
    });
  });

  it("stops observing when a destination does not mount by the deadline", async () => {
    vi.useFakeTimers();
    const getElementById = vi.spyOn(document, "getElementById");
    const view = render(
      <MemoryRouter initialEntries={["/#destination-that-never-mounts"]}>
        <HashNavigationScroll />
      </MemoryRouter>,
    );

    expect(getElementById).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(2_000);
    const callsAfterDeadline = getElementById.mock.calls.length;

    view.container.appendChild(document.createElement("div"));
    await Promise.resolve();

    expect(getElementById).toHaveBeenCalledTimes(callsAfterDeadline);
  });
});
