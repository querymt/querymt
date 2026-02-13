import { useState, useMemo } from 'react';
import { ElicitationData } from '../types';
import { useUiClientContext } from '../context/UiClientContext';
import { Check, Circle, Square, CheckSquare, Send, X, Ban } from 'lucide-react';

interface ElicitationCardProps {
  data: ElicitationData;
}

interface FormField {
  name: string;
  title?: string;
  description?: string;
  type: 'string' | 'number' | 'integer' | 'boolean' | 'array' | 'object';
  required: boolean;
  // For enum/oneOf single-select
  options?: { value: any; title?: string; description?: string }[];
  // For array multi-select
  itemOptions?: { value: any; title?: string; description?: string }[];
  // For simple types
  default?: any;
}

/**
 * Parse JSON Schema into form fields
 */
function parseSchema(schema: any): FormField[] {
  if (!schema || typeof schema !== 'object') return [];
  
  const properties = schema.properties || {};
  const required = new Set(schema.required || []);
  const fields: FormField[] = [];

  for (const [name, prop] of Object.entries(properties)) {
    const propSchema = prop as any;
    const field: FormField = {
      name,
      title: propSchema.title || name,
      description: propSchema.description,
      type: propSchema.type || 'string',
      required: required.has(name),
    };

    // Handle oneOf (single-select enum with titles)
    if (propSchema.oneOf && Array.isArray(propSchema.oneOf)) {
      field.options = propSchema.oneOf.map((opt: any) => ({
        value: opt.const ?? opt.value,
        title: opt.title,
        description: opt.description,
      }));
    }
    // Handle enum (simple enum without titles)
    else if (propSchema.enum && Array.isArray(propSchema.enum)) {
      field.options = propSchema.enum.map((value: any) => ({
        value,
        title: String(value),
      }));
    }
    // Handle array with anyOf/oneOf (multi-select)
    else if (propSchema.type === 'array' && propSchema.items) {
      const items = propSchema.items;
      if (items.anyOf && Array.isArray(items.anyOf)) {
        field.itemOptions = items.anyOf.map((opt: any) => ({
          value: opt.const ?? opt.value,
          title: opt.title,
          description: opt.description,
        }));
      } else if (items.oneOf && Array.isArray(items.oneOf)) {
        field.itemOptions = items.oneOf.map((opt: any) => ({
          value: opt.const ?? opt.value,
          title: opt.title,
          description: opt.description,
        }));
      }
    }

    fields.push(field);
  }

  return fields;
}

export function ElicitationCard({ data }: ElicitationCardProps) {
  const { sendElicitationResponse } = useUiClientContext();
  const [formState, setFormState] = useState<Record<string, any>>({});
  const [submitted, setSubmitted] = useState(false);
  const [action, setAction] = useState<'accept' | 'decline' | 'cancel' | null>(null);

  const fields = useMemo(() => parseSchema(data.requestedSchema), [data.requestedSchema]);

  const handleFieldChange = (fieldName: string, value: any) => {
    if (submitted) return;
    setFormState(prev => ({ ...prev, [fieldName]: value }));
  };

  const handleArrayToggle = (fieldName: string, value: any) => {
    if (submitted) return;
    setFormState(prev => {
      const current = new Set(prev[fieldName] || []);
      if (current.has(value)) {
        current.delete(value);
      } else {
        current.add(value);
      }
      return { ...prev, [fieldName]: Array.from(current) };
    });
  };

  const handleSubmit = (submitAction: 'accept' | 'decline' | 'cancel') => {
    if (submitted) return;

    // For accept, we need form data
    if (submitAction === 'accept') {
      // Validate required fields
      const missingRequired = fields.filter(f => f.required && !formState[f.name]);
      if (missingRequired.length > 0) {
        return; // Don't submit if required fields are missing
      }
      
      // Build content object from form state
      const content: Record<string, unknown> = {};
      for (const field of fields) {
        if (formState[field.name] !== undefined) {
          content[field.name] = formState[field.name];
        }
      }
      
      sendElicitationResponse(data.elicitationId, submitAction, content);
    } else {
      // For decline/cancel, no content needed
      sendElicitationResponse(data.elicitationId, submitAction);
    }
    
    setSubmitted(true);
    setAction(submitAction);
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSubmit('accept');
    }
  };

  // Check if form is valid for submission
  const canSubmit = fields.every(f => !f.required || formState[f.name] !== undefined);

  // Extract source label
  const sourceLabel = data.source.startsWith('mcp:') 
    ? data.source.substring(4) 
    : data.source === 'builtin:question' 
    ? 'built-in'
    : data.source;

  return (
    <div className="elicitation-card my-4 rounded-lg border border-cyber-cyan/30 bg-cyber-surface/60 overflow-hidden">
      {/* Header */}
      <div className="px-4 py-3 bg-cyber-cyan/10 border-b border-cyber-cyan/30">
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <div className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" />
            <h3 className="text-sm font-semibold text-cyber-cyan uppercase tracking-wide">
              Elicitation Request
            </h3>
          </div>
          {sourceLabel && (
            <span className="text-[10px] text-ui-muted px-2 py-0.5 rounded bg-cyber-surface/40 border border-cyber-border/30">
              via {sourceLabel}
            </span>
          )}
        </div>
      </div>

      {/* Message */}
      <div className="px-4 py-3 border-b border-cyber-border/30">
        <p className="text-sm text-ui-primary leading-relaxed">{data.message}</p>
      </div>

      {/* Form Fields */}
      <div className="px-4 py-3 space-y-4">
        {fields.map((field) => (
          <div key={field.name} className="space-y-2">
            {/* Field Label */}
            <div className="flex items-baseline gap-2">
              <label className="text-sm font-medium text-ui-secondary">
                {field.title || field.name}
                {field.required && <span className="text-cyber-orange ml-1">*</span>}
              </label>
            </div>
            
            {/* Field Description */}
            {field.description && (
              <p className="text-xs text-ui-secondary leading-relaxed">{field.description}</p>
            )}

            {/* Field Input */}
            {field.options ? (
              // Single-select with options (radio buttons)
              <div className="space-y-2">
                {field.options.map((option, idx) => {
                  const isSelected = formState[field.name] === option.value;
                  
                  return (
                    <button
                      key={idx}
                      onClick={() => handleFieldChange(field.name, option.value)}
                      disabled={submitted}
                      className={`
                        w-full text-left px-4 py-3 rounded-lg border transition-all duration-200
                        ${submitted 
                          ? 'cursor-default opacity-60' 
                          : 'cursor-pointer hover:border-cyber-cyan/50 hover:bg-cyber-cyan/5'
                        }
                        ${isSelected && !submitted
                          ? 'border-cyber-cyan/60 bg-cyber-cyan/10'
                          : 'border-cyber-border/40 bg-cyber-surface/40'
                        }
                        ${isSelected && submitted && action === 'accept'
                          ? 'border-cyber-lime/60 bg-cyber-lime/10'
                          : ''
                        }
                      `}
                    >
                      <div className="flex items-start gap-3">
                        <div className="flex-shrink-0 mt-0.5">
                          {isSelected ? (
                            <div className={`w-4 h-4 rounded-full border-2 flex items-center justify-center ${
                              submitted && action === 'accept' ? 'border-cyber-lime' : 'border-cyber-cyan'
                            }`}>
                              <div className={`w-2 h-2 rounded-full ${
                                submitted && action === 'accept' ? 'bg-cyber-lime' : 'bg-cyber-cyan'
                              }`} />
                            </div>
                          ) : (
                            <Circle className="w-4 h-4 text-ui-muted" />
                          )}
                        </div>
                        <div className="flex-1 min-w-0">
                          <div className={`text-sm font-medium ${
                            isSelected && submitted && action === 'accept' 
                              ? 'text-cyber-lime' 
                              : isSelected 
                              ? 'text-cyber-cyan' 
                              : 'text-ui-primary'
                          }`}>
                            {option.title || String(option.value)}
                          </div>
                          {option.description && (
                            <div className="text-xs text-ui-secondary mt-1 leading-relaxed">
                              {option.description}
                            </div>
                          )}
                        </div>
                        {isSelected && submitted && action === 'accept' && (
                          <Check className="w-4 h-4 text-cyber-lime flex-shrink-0 mt-0.5" />
                        )}
                      </div>
                    </button>
                  );
                })}
              </div>
            ) : field.itemOptions ? (
              // Multi-select with options (checkboxes)
              <div className="space-y-2">
                {field.itemOptions.map((option, idx) => {
                  const selected = new Set(formState[field.name] || []);
                  const isSelected = selected.has(option.value);
                  
                  return (
                    <button
                      key={idx}
                      onClick={() => handleArrayToggle(field.name, option.value)}
                      disabled={submitted}
                      className={`
                        w-full text-left px-4 py-3 rounded-lg border transition-all duration-200
                        ${submitted 
                          ? 'cursor-default opacity-60' 
                          : 'cursor-pointer hover:border-cyber-cyan/50 hover:bg-cyber-cyan/5'
                        }
                        ${isSelected && !submitted
                          ? 'border-cyber-cyan/60 bg-cyber-cyan/10'
                          : 'border-cyber-border/40 bg-cyber-surface/40'
                        }
                        ${isSelected && submitted && action === 'accept'
                          ? 'border-cyber-lime/60 bg-cyber-lime/10'
                          : ''
                        }
                      `}
                    >
                      <div className="flex items-start gap-3">
                        <div className="flex-shrink-0 mt-0.5">
                          {isSelected ? (
                            <CheckSquare className={`w-4 h-4 ${
                              submitted && action === 'accept' ? 'text-cyber-lime' : 'text-cyber-cyan'
                            }`} />
                          ) : (
                            <Square className="w-4 h-4 text-ui-muted" />
                          )}
                        </div>
                        <div className="flex-1 min-w-0">
                          <div className={`text-sm font-medium ${
                            isSelected && submitted && action === 'accept'
                              ? 'text-cyber-lime'
                              : isSelected
                              ? 'text-cyber-cyan'
                              : 'text-ui-primary'
                          }`}>
                            {option.title || String(option.value)}
                          </div>
                          {option.description && (
                            <div className="text-xs text-ui-secondary mt-1 leading-relaxed">
                              {option.description}
                            </div>
                          )}
                        </div>
                        {isSelected && submitted && action === 'accept' && (
                          <Check className="w-4 h-4 text-cyber-lime flex-shrink-0 mt-0.5" />
                        )}
                      </div>
                    </button>
                  );
                })}
              </div>
            ) : field.type === 'boolean' ? (
              // Boolean toggle
              <button
                onClick={() => handleFieldChange(field.name, !formState[field.name])}
                disabled={submitted}
                className={`
                  w-full text-left px-4 py-3 rounded-lg border transition-all duration-200
                  ${submitted 
                    ? 'cursor-default opacity-60' 
                    : 'cursor-pointer hover:border-cyber-cyan/50 hover:bg-cyber-cyan/5'
                  }
                  ${formState[field.name] && !submitted
                    ? 'border-cyber-cyan/60 bg-cyber-cyan/10'
                    : 'border-cyber-border/40 bg-cyber-surface/40'
                  }
                  ${formState[field.name] && submitted && action === 'accept'
                    ? 'border-cyber-lime/60 bg-cyber-lime/10'
                    : ''
                  }
                `}
              >
                <div className="flex items-center gap-3">
                  <div className="flex-shrink-0">
                    {formState[field.name] ? (
                      <CheckSquare className={`w-4 h-4 ${
                        submitted && action === 'accept' ? 'text-cyber-lime' : 'text-cyber-cyan'
                      }`} />
                    ) : (
                      <Square className="w-4 h-4 text-ui-muted" />
                    )}
                  </div>
                  <span className={`text-sm font-medium ${
                    formState[field.name] && submitted && action === 'accept'
                      ? 'text-cyber-lime'
                      : formState[field.name]
                      ? 'text-cyber-cyan'
                      : 'text-ui-primary'
                  }`}>
                    {formState[field.name] ? 'Yes' : 'No'}
                  </span>
                </div>
              </button>
            ) : field.type === 'number' || field.type === 'integer' ? (
              // Number input
              <input
                type="number"
                step={field.type === 'integer' ? '1' : 'any'}
                value={formState[field.name] ?? ''}
                onChange={(e) => {
                  const value = e.target.value === '' ? undefined : 
                    field.type === 'integer' ? parseInt(e.target.value, 10) : parseFloat(e.target.value);
                  handleFieldChange(field.name, value);
                }}
                onKeyDown={(e) => handleKeyDown(e)}
                disabled={submitted}
                className="w-full px-4 py-2.5 rounded-lg bg-cyber-bg border border-cyber-cyan/40 text-ui-primary text-sm placeholder:text-ui-muted focus:outline-none focus:ring-2 focus:ring-cyber-cyan/50 focus:border-cyber-cyan transition-all disabled:opacity-60 disabled:cursor-not-allowed"
                placeholder={field.type === 'integer' ? 'Enter a whole number...' : 'Enter a number...'}
              />
            ) : (
              // String input (default)
              <input
                type="text"
                value={formState[field.name] ?? ''}
                onChange={(e) => handleFieldChange(field.name, e.target.value)}
                onKeyDown={(e) => handleKeyDown(e)}
                disabled={submitted}
                className="w-full px-4 py-2.5 rounded-lg bg-cyber-bg border border-cyber-cyan/40 text-ui-primary text-sm placeholder:text-ui-muted focus:outline-none focus:ring-2 focus:ring-cyber-cyan/50 focus:border-cyber-cyan transition-all disabled:opacity-60 disabled:cursor-not-allowed"
                placeholder="Enter text..."
              />
            )}
          </div>
        ))}
      </div>

      {/* Action Buttons */}
      <div className="px-4 py-3 border-t border-cyber-border/30 bg-cyber-surface/40">
        {submitted ? (
          <div className="flex items-center justify-center gap-2 py-2">
            {action === 'accept' && (
              <>
                <Check className="w-4 h-4 text-cyber-lime" />
                <span className="text-sm font-medium text-cyber-lime">Response submitted</span>
              </>
            )}
            {action === 'decline' && (
              <>
                <X className="w-4 h-4 text-cyber-orange" />
                <span className="text-sm font-medium text-cyber-orange">Request declined</span>
              </>
            )}
            {action === 'cancel' && (
              <>
                <Ban className="w-4 h-4 text-ui-secondary" />
                <span className="text-sm font-medium text-ui-secondary">Request cancelled</span>
              </>
            )}
          </div>
        ) : (
          <div className="flex gap-2">
            {/* Decline button */}
            <button
              onClick={() => handleSubmit('decline')}
              className="flex-1 flex items-center justify-center gap-2 px-4 py-2.5 rounded-lg font-medium text-sm transition-all duration-200 bg-cyber-surface/60 border border-cyber-orange/40 text-cyber-orange hover:bg-cyber-orange/10 hover:border-cyber-orange"
            >
              <X className="w-4 h-4" />
              <span>Decline</span>
            </button>

            {/* Submit (Accept) button */}
            <button
              onClick={() => handleSubmit('accept')}
              disabled={!canSubmit}
              className={`
                flex-1 flex items-center justify-center gap-2 px-4 py-2.5 rounded-lg font-medium text-sm
                transition-all duration-200
                ${canSubmit
                  ? 'bg-cyber-cyan/20 border border-cyber-cyan/60 text-cyber-cyan hover:bg-cyber-cyan/30 hover:border-cyber-cyan cursor-pointer'
                  : 'bg-cyber-surface/60 border border-cyber-border/40 text-ui-muted cursor-not-allowed opacity-50'
                }
              `}
            >
              <Send className="w-4 h-4" />
              <span>Submit</span>
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
