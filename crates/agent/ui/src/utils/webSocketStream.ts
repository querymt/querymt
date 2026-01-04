import type { Stream } from '@agentclientprotocol/sdk';
import type { AnyMessage } from '@agentclientprotocol/sdk';

/**
 * Creates a bidirectional Stream from a WebSocket connection.
 * 
 * This adapter allows the ACP TypeScript SDK to communicate over WebSocket
 * by providing ReadableStream/WritableStream interfaces.
 * 
 * Features:
 * - Auto-reconnect with exponential backoff
 * - Message queuing during connection
 * - Proper error handling
 */
type WebSocketStreamOptions = {
  onOpen?: () => void;
  onClose?: () => void;
  onError?: (event: Event) => void;
};

export function createWebSocketStream(
  url: string,
  options: WebSocketStreamOptions = {}
): Stream {
  let ws: WebSocket | null = null;
  const messageQueue: AnyMessage[] = [];
  let readController: ReadableStreamDefaultController<AnyMessage> | null = null;
  let reconnectAttempts = 0;
  const maxReconnectDelay = 30000; // 30 seconds max
  
  function connect() {
    ws = new WebSocket(url);
    
    ws.addEventListener('open', () => {
      console.log('WebSocket connected');
      reconnectAttempts = 0;
      options.onOpen?.();
      
      // Flush queued messages
      while (messageQueue.length > 0 && ws?.readyState === WebSocket.OPEN) {
        const message = messageQueue.shift()!;
        ws.send(JSON.stringify(message));
      }
    });

    ws.addEventListener('message', (event) => {
      try {
        const message = JSON.parse(event.data) as AnyMessage;
        if (readController) {
          readController.enqueue(message);
        } else {
          // Queue messages received before readable stream is ready
          messageQueue.push(message);
        }
      } catch (err) {
        console.error('Failed to parse WebSocket message:', err);
      }
    });

    ws.addEventListener('close', () => {
      console.log('WebSocket closed, attempting reconnect...');
      options.onClose?.();
      attemptReconnect();
    });

    ws.addEventListener('error', (err) => {
      console.error('WebSocket error:', err);
      options.onError?.(err);
      // Error will trigger close event, which handles reconnection
    });
  }
  
  function attemptReconnect() {
    // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 30s (max)
    const delay = Math.min(1000 * Math.pow(2, reconnectAttempts), maxReconnectDelay);
    reconnectAttempts++;
    
    console.log(`Reconnecting in ${delay}ms (attempt ${reconnectAttempts})...`);
    setTimeout(() => {
      connect();
    }, delay);
  }

  // Initial connection
  connect();

  // Readable stream (server → client)
  const readable = new ReadableStream<AnyMessage>({
    start(controller) {
      readController = controller;
      
      // Flush any messages that arrived before stream was ready
      while (messageQueue.length > 0) {
        controller.enqueue(messageQueue.shift()!);
      }
    },
    
    cancel() {
      // Clean up when stream is cancelled
      if (ws) {
        ws.close();
        ws = null;
      }
    }
  });

  // Writable stream (client → server)
  const writable = new WritableStream<AnyMessage>({
    async write(message) {
      if (!ws) {
        throw new Error('WebSocket is not initialized');
      }
      
      // Wait for WebSocket to be open
      if (ws.readyState === WebSocket.CONNECTING) {
        await new Promise<void>((resolve, reject) => {
          const checkInterval = setInterval(() => {
            if (!ws) {
              clearInterval(checkInterval);
              reject(new Error('WebSocket closed during connection'));
              return;
            }
            if (ws.readyState === WebSocket.OPEN) {
              clearInterval(checkInterval);
              resolve();
            } else if (ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
              clearInterval(checkInterval);
              reject(new Error('WebSocket closed before opening'));
            }
          }, 100);
        });
      }

      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(message));
      } else {
        // Queue message if connection is lost
        console.warn('WebSocket not open, queuing message');
        messageQueue.push(message);
      }
    },
    
    close() {
      if (ws) {
        ws.close();
        ws = null;
      }
    },
    
    abort(reason) {
      console.error('WebSocket stream aborted:', reason);
      if (ws) {
        ws.close();
        ws = null;
      }
    }
  });

  return { readable, writable };
}
