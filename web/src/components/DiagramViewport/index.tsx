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

function clampScale(scale: number): number {
  return Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale));
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
      const availableWidth = Math.max(viewportRect.width - viewportPadding * 2, 1);
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
        excludeClass: 'clickable',
        handleStartEvent: (event: Event) => {
          const target = event.target;
          if (target instanceof Element && target.closest('a, .clickable')) {
            return;
          }

          event.preventDefault();
          event.stopPropagation();
        },
        maxScale: MAX_SCALE,
        minScale: MIN_SCALE,
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
      const delta = event.deltaY === 0 && event.deltaX ? event.deltaX : event.deltaY;
      const wheel = delta < 0 ? 1 : -1;
      const nextScale = currentScale * Math.exp((wheel * ZOOM_STEP) / 3);
      zoomToViewportPoint(nextScale, event);
    };

    viewport.addEventListener('wheel', handleWheel, {passive: false});

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
