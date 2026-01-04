import type { 
  Client,
  RequestPermissionRequest,
  RequestPermissionResponse,
  SessionNotification,
} from '@agentclientprotocol/sdk';

/**
 * Browser-based implementation of the ACP Client interface.
 * 
 * This handles callbacks from the agent, such as:
 * - Session updates (messages, tool calls, etc.)
 * - Permission requests for tool execution
 */
export class BrowserClient implements Client {
  private eventHandlers: ((notification: SessionNotification) => void)[] = [];

  /**
   * Handle session updates from the agent.
   * 
   * This is called for every update: user messages, agent messages,
   * tool calls, tool results, etc.
   */
  async sessionUpdate(params: SessionNotification): Promise<void> {
    // Notify all registered handlers
    this.eventHandlers.forEach(handler => {
      try {
        handler(params);
      } catch (err) {
        console.error('Error in session update handler:', err);
      }
    });
  }

  /**
   * Handle permission requests from the agent.
   * 
   * TODO: Implement UI dialog for user approval.
   * For now, auto-approves all permission requests.
   */
  async requestPermission(
    params: RequestPermissionRequest
  ): Promise<RequestPermissionResponse> {
    console.log('Permission requested:', params);
    
    // TODO: Show UI modal/dialog to user with options
    // User should be able to:
    // - Approve (select an option)
    // - Deny
    // - Approve all (remember choice)
    
    // For now, auto-approve by selecting first option
    const firstOption = params.options[0];
    if (!firstOption) {
      console.error('No options provided in permission request');
      // Return cancelled if no options
      return {
        outcome: {
          outcome: 'cancelled'
        }
      };
    }
    
    return {
      outcome: {
        outcome: 'selected',
        optionId: firstOption.optionId
      }
    };
  }

  /**
   * Register a handler for session updates.
   * 
   * Multiple handlers can be registered and all will be called.
   */
  onSessionUpdate(handler: (notification: SessionNotification) => void): void {
    this.eventHandlers.push(handler);
  }
  
  /**
   * Remove a session update handler.
   */
  offSessionUpdate(handler: (notification: SessionNotification) => void): void {
    const index = this.eventHandlers.indexOf(handler);
    if (index !== -1) {
      this.eventHandlers.splice(index, 1);
    }
  }
}
