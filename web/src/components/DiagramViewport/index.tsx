import type Panzoom from '@panzoom/panzoom';
import type {PanzoomObject} from '@panzoom/panzoom';
import clsx from 'clsx';
import React, {
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from 'react';

import styles from './styles.module.css';

const MIN_SCALE = 0.35;
const MAX_SCALE = 3;
const ZOOM_STEP = 0.25;
const VIEWPORT_PADDING = 48;
const DEFAULT_SVG_WIDTH = 1200;
const DEFAULT_SVG_HEIGHT = 700;

type DiagramViewportProps = {
  children: ReactNode;
  initialPlacement?: 'center' | 'center-top';
  viewportPadding?: number;
};

type ViewportTransform = {
  scale: number;
  x: number;
  y: number;
};

type GesturePoint = {
  clientX: number;
  clientY: number;
  pointerId: number;
};

type DragGesture = {
  pointerId: number;
  startClientX: number;
  startClientY: number;
  startPan: {
    x: number;
    y: number;
  };
};

type PinchGesture = {
  startDistance: number;
  startMiddle: {
    clientX: number;
    clientY: number;
  };
  startPan: {
    x: number;
    y: number;
  };
  startScale: number;
};

function clampScale(scale: number): number {
  return Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale));
}

function getPointerDistance(first: GesturePoint, second: GesturePoint): number {
  return Math.hypot(
    second.clientX - first.clientX,
    second.clientY - first.clientY,
  );
}

function getPointerMiddle(first: GesturePoint, second: GesturePoint) {
  return {
    clientX: (first.clientX + second.clientX) / 2,
    clientY: (first.clientY + second.clientY) / 2,
  };
}

export default function DiagramViewport({
  children,
  initialPlacement = 'center-top',
  viewportPadding = VIEWPORT_PADDING,
}: DiagramViewportProps): ReactNode {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  const panzoomRef = useRef<PanzoomObject | null>(null);
  const initialTransformRef = useRef<ViewportTransform | null>(null);
  const contentSizeRef = useRef<{height: number; width: number} | null>(null);
  const [isReady, setIsReady] = useState(false);
  const [canFullscreen, setCanFullscreen] = useState(false);

  const zoomToViewportPoint = useCallback(
    (
      nextScale: number,
      point: {
        clientX: number;
        clientY: number;
      },
    ) => {
      const viewport = viewportRef.current;
      const panzoom = panzoomRef.current;
      if (!viewport || !panzoom) {
        return;
      }

      const rect = viewport.getBoundingClientRect();
      const focalX = point.clientX - rect.left;
      const focalY = point.clientY - rect.top;
      const currentScale = panzoom.getScale();
      const currentPan = panzoom.getPan();
      const scale = clampScale(nextScale);

      if (scale === currentScale) {
        return;
      }

      const nextPanX =
        currentPan.x + focalX / scale - focalX / currentScale;
      const nextPanY =
        currentPan.y + focalY / scale - focalY / currentScale;

      panzoom.zoom(scale, {
        animate: false,
        force: true,
      });
      panzoom.pan(nextPanX, nextPanY, {
        animate: false,
        force: true,
      });
    },
    [],
  );

  useEffect(() => {
    setCanFullscreen(
      typeof document !== 'undefined' &&
        Boolean(document.fullscreenEnabled),
    );
  }, []);

  useEffect(() => {
    if (typeof window === 'undefined') {
      return undefined;
    }

    const viewport = viewportRef.current;
    const content = contentRef.current;
    if (!viewport || !content) {
      return undefined;
    }

    let disposed = false;
    let PanzoomCtor: typeof Panzoom | null = null;

    const getSvgSize = (svg: SVGSVGElement) => {
      const viewBox = svg.viewBox.baseVal;
      if (viewBox.width > 0 && viewBox.height > 0) {
        return {
          height: viewBox.height,
          width: viewBox.width,
        };
      }

      const width = svg.width.baseVal.value;
      const height = svg.height.baseVal.value;
      if (width > 0 && height > 0) {
        return {height, width};
      }

      const rect = svg.getBoundingClientRect();
      return {
        height: Math.max(rect.height, DEFAULT_SVG_HEIGHT),
        width: Math.max(rect.width, DEFAULT_SVG_WIDTH),
      };
    };

    const prepareSvg = (svg: SVGSVGElement) => {
      const {height, width} = getSvgSize(svg);
      svg.removeAttribute('width');
      svg.removeAttribute('height');
      svg.style.width = `${width}px`;
      svg.style.height = `${height}px`;
      svg.style.maxWidth = 'none';
      content.style.width = `${width}px`;
      content.style.minWidth = `${width}px`;
      content.style.height = `${height}px`;

      return {height, width};
    };

    const getInitialTransform = (width: number, height: number) => {
      const viewportRect = viewport.getBoundingClientRect();
      const availableWidth = Math.max(
        viewportRect.width - viewportPadding * 2,
        1,
      );
      const availableHeight = Math.max(
        viewportRect.height - viewportPadding * 2,
        1,
      );
      const fitScale = Math.min(
        1,
        Math.max(
          MIN_SCALE,
          Math.min(availableWidth / width, availableHeight / height),
        ),
      );

      return {
        scale: fitScale,
        x: (viewportRect.width - width * fitScale) / 2,
        y:
          initialPlacement === 'center-top'
            ? viewportPadding
            : (viewportRect.height - height * fitScale) / 2,
      };
    };

    const applyInitialTransform = (transform: ViewportTransform) => {
      const panX = transform.x / transform.scale;
      const panY = transform.y / transform.scale;

      panzoomRef.current?.zoom(transform.scale, {
        animate: false,
        force: true,
      });
      panzoomRef.current?.pan(panX, panY, {
        animate: false,
        force: true,
      });
    };

    const destroyPanzoom = (updateState = true) => {
      panzoomRef.current?.destroy();
      panzoomRef.current = null;
      if (updateState) {
        setIsReady(false);
      }
    };

    const initializePanzoom = () => {
      if (disposed || panzoomRef.current || !PanzoomCtor) {
        return;
      }

      const svg = content.querySelector<SVGSVGElement>('svg');
      if (!svg) {
        return;
      }

      const {height, width} = prepareSvg(svg);
      contentSizeRef.current = {height, width};
      const initial = getInitialTransform(width, height);
      initialTransformRef.current = initial;

      panzoomRef.current = PanzoomCtor(content, {
        animate: false,
        canvas: true,
        cursor: 'grab',
        maxScale: MAX_SCALE,
        minScale: MIN_SCALE,
        noBind: true,
        origin: '0 0',
        overflow: 'hidden',
        startScale: initial.scale,
        startX: initial.x / initial.scale,
        startY: initial.y / initial.scale,
        step: ZOOM_STEP,
        touchAction: 'none',
      });
      applyInitialTransform(initial);
      panzoomRef.current.setOptions({animate: true});
      setIsReady(true);
    };

    const handleWheel = (event: WheelEvent) => {
      if (!panzoomRef.current) {
        return;
      }

      event.preventDefault();
      const currentScale = panzoomRef.current.getScale();
      const delta =
        event.deltaY === 0 && event.deltaX ? event.deltaX : event.deltaY;
      const wheel = delta < 0 ? 1 : -1;
      const nextScale = currentScale * Math.exp((wheel * ZOOM_STEP) / 3);
      zoomToViewportPoint(nextScale, event);
    };

    const activePointers = new Map<number, GesturePoint>();
    let dragGesture: DragGesture | null = null;
    let pinchGesture: PinchGesture | null = null;

    const shouldIgnorePointerTarget = (target: EventTarget | null) =>
      target instanceof Element && Boolean(target.closest('a, .clickable'));

    const getFirstPointers = () => {
      const [first, second] = Array.from(activePointers.values());
      if (!first || !second) {
        return null;
      }

      return {first, second};
    };

    const startDragGesture = (pointer: GesturePoint) => {
      const panzoom = panzoomRef.current;
      if (!panzoom) {
        return;
      }

      dragGesture = {
        pointerId: pointer.pointerId,
        startClientX: pointer.clientX,
        startClientY: pointer.clientY,
        startPan: panzoom.getPan(),
      };
    };

    const startPinchGesture = () => {
      const panzoom = panzoomRef.current;
      const pointers = getFirstPointers();
      if (!panzoom || !pointers) {
        pinchGesture = null;
        return;
      }

      const distance = getPointerDistance(pointers.first, pointers.second);
      if (distance === 0) {
        pinchGesture = null;
        return;
      }

      pinchGesture = {
        startDistance: distance,
        startMiddle: getPointerMiddle(pointers.first, pointers.second),
        startPan: panzoom.getPan(),
        startScale: panzoom.getScale(),
      };
    };

    const handlePointerDown = (event: PointerEvent) => {
      const panzoom = panzoomRef.current;
      if (!panzoom || shouldIgnorePointerTarget(event.target)) {
        return;
      }

      event.preventDefault();
      event.stopPropagation();
      if (!viewport.hasPointerCapture(event.pointerId)) {
        viewport.setPointerCapture(event.pointerId);
      }

      const pointer = {
        clientX: event.clientX,
        clientY: event.clientY,
        pointerId: event.pointerId,
      };
      activePointers.set(event.pointerId, pointer);

      if (activePointers.size >= 2) {
        dragGesture = null;
        startPinchGesture();
        return;
      }

      pinchGesture = null;
      startDragGesture(pointer);
    };

    const handlePointerMove = (event: PointerEvent) => {
      const pointer = activePointers.get(event.pointerId);
      const panzoom = panzoomRef.current;
      if (!pointer || !panzoom) {
        return;
      }

      event.preventDefault();
      activePointers.set(event.pointerId, {
        clientX: event.clientX,
        clientY: event.clientY,
        pointerId: event.pointerId,
      });

      if (activePointers.size >= 2) {
        const pointers = getFirstPointers();
        if (!pointers) {
          return;
        }

        if (!pinchGesture) {
          startPinchGesture();
        }

        if (!pinchGesture) {
          return;
        }

        const currentDistance = getPointerDistance(
          pointers.first,
          pointers.second,
        );
        if (currentDistance === 0) {
          return;
        }

        const rect = viewport.getBoundingClientRect();
        const currentMiddle = getPointerMiddle(pointers.first, pointers.second);
        const currentMiddleX = currentMiddle.clientX - rect.left;
        const currentMiddleY = currentMiddle.clientY - rect.top;
        const startMiddleX = pinchGesture.startMiddle.clientX - rect.left;
        const startMiddleY = pinchGesture.startMiddle.clientY - rect.top;
        const nextScale = clampScale(
          (pinchGesture.startScale * currentDistance) /
            pinchGesture.startDistance,
        );

        panzoom.zoom(nextScale, {
          animate: false,
          force: true,
        });
        // Keep the diagram point that began under the pinch midpoint
        // under the current midpoint as the fingers move.
        panzoom.pan(
          pinchGesture.startPan.x +
            currentMiddleX / nextScale -
            startMiddleX / pinchGesture.startScale,
          pinchGesture.startPan.y +
            currentMiddleY / nextScale -
            startMiddleY / pinchGesture.startScale,
          {
            animate: false,
            force: true,
          },
        );
        return;
      }

      if (!dragGesture || dragGesture.pointerId !== event.pointerId) {
        return;
      }

      const currentScale = panzoom.getScale();
      panzoom.pan(
        dragGesture.startPan.x +
          (event.clientX - dragGesture.startClientX) / currentScale,
        dragGesture.startPan.y +
          (event.clientY - dragGesture.startClientY) / currentScale,
        {
          animate: false,
          force: true,
        },
      );
    };

    const handlePointerUp = (event: PointerEvent) => {
      activePointers.delete(event.pointerId);
      if (viewport.hasPointerCapture(event.pointerId)) {
        viewport.releasePointerCapture(event.pointerId);
      }

      if (activePointers.size >= 2) {
        startPinchGesture();
        return;
      }

      pinchGesture = null;
      const [remainingPointer] = Array.from(activePointers.values());
      if (remainingPointer) {
        startDragGesture(remainingPointer);
      } else {
        dragGesture = null;
      }
    };

    viewport.addEventListener('wheel', handleWheel, {passive: false});
    viewport.addEventListener('pointerdown', handlePointerDown, {
      passive: false,
    });
    viewport.addEventListener('pointermove', handlePointerMove, {
      passive: false,
    });
    viewport.addEventListener('pointerup', handlePointerUp);
    viewport.addEventListener('pointercancel', handlePointerUp);

    void import('@panzoom/panzoom').then((module) => {
      if (disposed) {
        return;
      }

      PanzoomCtor = module.default;
      initializePanzoom();
    });

    const observer = new MutationObserver(() => {
      if (panzoomRef.current && !content.querySelector('svg')) {
        destroyPanzoom();
        contentSizeRef.current = null;
        initialTransformRef.current = null;
      }

      initializePanzoom();
    });

    observer.observe(content, {childList: true, subtree: true});

    const resizeObserver = new ResizeObserver(() => {
      const size = contentSizeRef.current;
      if (!size || !panzoomRef.current) {
        return;
      }

      const initial = getInitialTransform(size.width, size.height);
      initialTransformRef.current = initial;
      applyInitialTransform(initial);
    });

    resizeObserver.observe(viewport);
    initializePanzoom();

    return () => {
      disposed = true;
      viewport.removeEventListener('wheel', handleWheel);
      viewport.removeEventListener('pointerdown', handlePointerDown);
      viewport.removeEventListener('pointermove', handlePointerMove);
      viewport.removeEventListener('pointerup', handlePointerUp);
      viewport.removeEventListener('pointercancel', handlePointerUp);
      observer.disconnect();
      resizeObserver.disconnect();
      destroyPanzoom(false);
    };
  }, [initialPlacement, viewportPadding, zoomToViewportPoint]);

  const reset = useCallback(() => {
    const initialTransform = initialTransformRef.current;
    if (!initialTransform || !panzoomRef.current) {
      return;
    }

    const panX = initialTransform.x / initialTransform.scale;
    const panY = initialTransform.y / initialTransform.scale;

    panzoomRef.current.zoom(initialTransform.scale, {
      animate: true,
      force: true,
    });
    panzoomRef.current.pan(panX, panY, {
      animate: true,
      force: true,
    });
  }, []);

  const zoomAtViewportCenter = useCallback((direction: 'in' | 'out') => {
    const viewport = viewportRef.current;
    const panzoom = panzoomRef.current;
    if (!viewport || !panzoom) {
      return;
    }

    const currentScale = panzoom.getScale();
    const nextScale =
      direction === 'in'
        ? currentScale * (1 + ZOOM_STEP)
        : currentScale / (1 + ZOOM_STEP);

    const rect = viewport.getBoundingClientRect();
    zoomToViewportPoint(nextScale, {
      clientX: rect.left + rect.width / 2,
      clientY: rect.top + rect.height / 2,
    });
  }, [zoomToViewportPoint]);

  const zoomIn = useCallback(() => {
    zoomAtViewportCenter('in');
  }, [zoomAtViewportCenter]);

  const zoomOut = useCallback(() => {
    zoomAtViewportCenter('out');
  }, [zoomAtViewportCenter]);

  const toggleFullscreen = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport || typeof document === 'undefined') {
      return;
    }

    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      void viewport.requestFullscreen();
    }
  }, []);

  return (
    <div className={styles.frame}>
      <div className={styles.toolbar} aria-label="Diagram controls">
        <button
          type="button"
          className={styles.control}
          onClick={zoomOut}
          disabled={!isReady}
          aria-label="Zoom out"
          title="Zoom out">
          -
        </button>
        <button
          type="button"
          className={styles.control}
          onClick={reset}
          disabled={!isReady}
          aria-label="Reset zoom"
          title="Reset zoom">
          Reset
        </button>
        <button
          type="button"
          className={styles.control}
          onClick={zoomIn}
          disabled={!isReady}
          aria-label="Zoom in"
          title="Zoom in">
          +
        </button>
        {canFullscreen && (
          <button
            type="button"
            className={clsx(styles.control, styles.fullscreen)}
            onClick={toggleFullscreen}
            aria-label="Toggle fullscreen"
            title="Fullscreen">
            Fullscreen
          </button>
        )}
      </div>
      <div ref={viewportRef} className={styles.viewport}>
        <div ref={contentRef} className={styles.content}>
          {children}
        </div>
      </div>
    </div>
  );
}
