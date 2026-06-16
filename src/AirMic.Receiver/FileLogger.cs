using System;
using System.IO;

namespace AirMic.Receiver
{
    public static class FileLogger
    {
        private static readonly object _lock = new object();
        private static readonly string _logDirectory = Path.Combine(AppContext.BaseDirectory, "logs");

        public static void Log(string message, string level = "INFO", Exception? exception = null)
        {
            lock (_lock)
            {
                try
                {
                    if (!Directory.Exists(_logDirectory))
                    {
                        Directory.CreateDirectory(_logDirectory);
                    }

                    string fileName = $"receiver_{DateTime.Now:yyyy-MM-dd}.log";
                    string filePath = Path.Combine(_logDirectory, fileName);

                    string logText = $"[{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff}] [{level}] {message}";
                    if (exception != null)
                    {
                        logText += Environment.NewLine + exception.ToString();
                    }

                    File.AppendAllText(filePath, logText + Environment.NewLine);
                }
                catch
                {
                    // Ignore logging errors to prevent application crash
                }
            }
        }
    }
}
